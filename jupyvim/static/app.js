let cells = [];
const cellExecMap = new Map(); // msg_id -> cell_index
const cellTimers = new Map(); // cell_index -> { intervalId, startTime }
let ws = null;

// Starts the live "N.Ns" timer shown next to the exec count while a cell is
// running (mirrors VS Code's notebook execution indicator).
function startCellTimer(index) {
    stopCellTimer(index);
    const statusEl = document.getElementById(`exec-status-${index}`);
    const countEl = document.querySelector(`#cell-${index} .exec-count`);
    if (countEl) countEl.textContent = '[*]';
    if (statusEl) {
        statusEl.className = 'exec-status running';
        statusEl.textContent = '0.0s';
    }
    const startTime = performance.now();
    const intervalId = setInterval(() => {
        if (statusEl) statusEl.textContent = ((performance.now() - startTime) / 1000).toFixed(1) + 's';
    }, 100);
    cellTimers.set(index, { intervalId, startTime });
}

// Stops the timer. With `result` given ({success, execCount}), leaves a
// persistent ✓/✗ + final duration in place (VS Code shows this until the
// next run, not just a brief flash) and updates the execution count.
function stopCellTimer(index, result) {
    const timer = cellTimers.get(index);
    let elapsed = 0;
    if (timer) {
        clearInterval(timer.intervalId);
        elapsed = (performance.now() - timer.startTime) / 1000;
        cellTimers.delete(index);
    }

    const statusEl = document.getElementById(`exec-status-${index}`);
    const countEl = document.querySelector(`#cell-${index} .exec-count`);
    if (!result) {
        if (statusEl) { statusEl.className = 'exec-status'; statusEl.textContent = ''; }
        return;
    }
    if (statusEl) {
        statusEl.className = 'exec-status ' + (result.success ? 'success' : 'error');
        statusEl.textContent = (result.success ? '✓ ' : '✗ ') + elapsed.toFixed(1) + 's';
    }
    if (countEl && result.execCount != null) countEl.textContent = `[${result.execCount}]`;
}

// Fetch the current (server-rendered) notebook state and render it.
async function initNotebook() {
    try {
        const res = await fetch('/api/notebook');
        if (!res.ok) throw new Error('Failed to load notebook data');
        const data = await res.json();
        cells = data.cells || [];

        document.getElementById('filename-display').textContent = data.filename || 'Untitled.ipynb';
        document.title = `Jupyvim - ${data.filename || 'Untitled'}`;

        renderNotebook();
    } catch (err) {
        console.error('Error fetching notebook:', err);
        document.getElementById('cells-container').innerHTML = `
            <div class="cell" style="padding: 16px; border-color: var(--error-border);">
                <h3 style="color: var(--error-text);">Failed to load notebook</h3>
                <p>${err.message}</p>
            </div>
        `;
    }
}

// Incremental render: while live-typing sync fires (every ~120ms pause),
// most updates change cell *content*, not cell *count/order*. A full
// innerHTML rebuild on every keystroke pause would flicker, reset scroll
// position, and blow away a currently-streaming output of an unrelated cell
// that happens to be mid-execution. So: patch existing cells in place when
// their type hasn't changed, and only touch the DOM structurally (create,
// replace, remove) when it actually needs to change.
function renderNotebook() {
    const container = document.getElementById('cells-container');
    const previousCount = container.children.length;

    cells.forEach((cell, idx) => {
        const existing = document.getElementById(`cell-${idx}`);
        if (existing && existing.dataset.cellType === cell.cell_type) {
            patchCellContent(existing, cell, idx);
        } else {
            const fresh = createCellElement(cell, idx);
            if (existing) {
                container.replaceChild(fresh, existing);
            } else {
                container.appendChild(fresh);
            }
        }
    });

    for (let idx = cells.length; idx < previousCount; idx++) {
        const stale = document.getElementById(`cell-${idx}`);
        if (stale) stale.remove();
    }
}

// Updates an existing cell's content without touching its running/status
// state. Skips output/exec-count updates while a cell is mid-execution --
// that DOM is owned by the live iopub stream until it goes idle.
function patchCellContent(cellEl, cell, index) {
    const isRunning = cellEl.classList.contains('running');

    cellEl.dataset.lineStart = cell.line_start;
    const jumpBtn = cellEl.querySelector('.cell-jump-btn');
    if (jumpBtn) jumpBtn.setAttribute('onclick', `jumpToLine(${cell.line_start})`);

    if (cell.cell_type === 'code') {
        const codeEl = cellEl.querySelector('.cell-code');
        if (codeEl) codeEl.innerHTML = cell.source_html || escapeHtml(cell.source);
        if (!isRunning) {
            const execEl = cellEl.querySelector('.exec-count');
            if (execEl) execEl.textContent = `[${cell.execution_count != null ? cell.execution_count : ' '}]`;
            const outputContainer = document.getElementById(`output-${index}`);
            if (outputContainer) {
                outputContainer.innerHTML = '';
                (cell.outputs || []).forEach(out => renderSavedOutput(outputContainer, out));
            }
        }
    } else if (cell.cell_type === 'raw') {
        const codeEl = cellEl.querySelector('.cell-code');
        if (codeEl) codeEl.textContent = cell.source || '';
    } else {
        const mdView = cellEl.querySelector('.markdown-rendered');
        if (mdView) mdView.innerHTML = cell.html || '<em>Empty markdown cell.</em>';
    }
}

// Browser -> Neovim: sends the mirror-file line number for a cell over the
// already-open WebSocket. The server shells out to `nvim --server
// --remote-expr` to move the cursor there -- see server.rs.
function jumpToLine(line) {
    if (ws && ws.readyState === WebSocket.OPEN && line != null) {
        ws.send(JSON.stringify({ event: 'jump_to_line', line }));
    }
}

function createCellElement(cell, index) {
    const cellEl = document.createElement('div');
    cellEl.className = `cell ${cell.cell_type}`;
    cellEl.id = `cell-${index}`;
    cellEl.dataset.cellType = cell.cell_type;
    cellEl.dataset.lineStart = cell.line_start;

    const header = document.createElement('div');
    header.className = 'cell-header';
    header.innerHTML = `
        <span class="cell-type-label">${cell.cell_type}</span>
        <button class="cell-jump-btn" title="Jump to code in Neovim" onclick="jumpToLine(${cell.line_start})">↦ code</button>
    `;
    cellEl.appendChild(header);

    const body = document.createElement('div');
    body.className = 'cell-body';

    const gutter = document.createElement('div');
    gutter.className = 'cell-gutter';
    if (cell.cell_type === 'code') {
        gutter.innerHTML = `
            <button class="run-cell-btn" title="Run Cell" onclick="runCell(${index})">▶</button>
            <span class="exec-count">[${cell.execution_count != null ? cell.execution_count : ' '}]</span>
            <span class="exec-status" id="exec-status-${index}"></span>
        `;
    } else {
        gutter.innerHTML = `<span class="exec-count">${cell.cell_type === 'raw' ? 'RAW' : 'MD'}</span>`;
    }
    body.appendChild(gutter);

    const contentEl = document.createElement('div');
    contentEl.className = 'cell-editor-container';
    if (cell.cell_type === 'code') {
        const pre = document.createElement('pre');
        pre.className = 'cell-code';
        pre.innerHTML = cell.source_html || escapeHtml(cell.source);
        contentEl.appendChild(pre);
    } else if (cell.cell_type === 'raw') {
        const pre = document.createElement('pre');
        pre.className = 'cell-code';
        pre.textContent = cell.source || '';
        contentEl.appendChild(pre);
    } else {
        const mdView = document.createElement('div');
        mdView.className = 'markdown-rendered';
        mdView.innerHTML = cell.html || '<em>Empty markdown cell.</em>';
        contentEl.appendChild(mdView);
    }
    body.appendChild(contentEl);
    cellEl.appendChild(body);

    if (cell.cell_type === 'code') {
        const outputContainer = document.createElement('div');
        outputContainer.className = 'cell-output';
        outputContainer.id = `output-${index}`;
        (cell.outputs || []).forEach(out => renderSavedOutput(outputContainer, out));
        cellEl.appendChild(outputContainer);
    }

    return cellEl;
}

function escapeHtml(text) {
    const div = document.createElement('div');
    div.textContent = text || '';
    return div.innerHTML;
}

// Click-to-zoom overlay for plot images (VS Code's "Plot Viewer", minus the
// dedicated tab/window -- a lightbox is much lighter and just as usable).
// One overlay element is created lazily and reused for every image.
function openImageLightbox(src) {
    let overlay = document.getElementById('jupyvim-lightbox');
    if (!overlay) {
        overlay = document.createElement('div');
        overlay.id = 'jupyvim-lightbox';
        overlay.className = 'lightbox-overlay';
        overlay.onclick = () => overlay.classList.remove('open');
        const img = document.createElement('img');
        img.id = 'jupyvim-lightbox-img';
        overlay.appendChild(img);
        document.body.appendChild(overlay);
        document.addEventListener('keydown', (e) => {
            if (e.key === 'Escape') overlay.classList.remove('open');
        });
    }
    document.getElementById('jupyvim-lightbox-img').src = src;
    overlay.classList.add('open');
}

// Builds a plot image with hover buttons (copy to clipboard, save to disk)
// and click-to-zoom, mirroring VS Code's notebook output image toolbar.
// Right-click already gives "copy image" / "save as" natively in any real
// browser -- these buttons are just parity with VS Code's one-click UX,
// built entirely on standard web APIs (Clipboard API, <a download>).
function createImageOutput(base64png) {
    const wrap = document.createElement('div');
    wrap.className = 'output-image-wrap';

    const img = document.createElement('img');
    img.className = 'output-image';
    img.src = `data:image/png;base64,${base64png.replace(/\n/g, '')}`;
    img.title = 'Click to zoom';
    img.onclick = () => openImageLightbox(img.src);
    wrap.appendChild(img);

    const copyBtn = document.createElement('button');
    copyBtn.className = 'output-image-btn output-image-copy';
    copyBtn.title = 'Copy image to clipboard';
    copyBtn.textContent = '⧉';
    copyBtn.onclick = async (e) => {
        e.stopPropagation();
        try {
            const blob = await (await fetch(img.src)).blob();
            await navigator.clipboard.write([new ClipboardItem({ [blob.type]: blob })]);
            copyBtn.textContent = '✓';
            setTimeout(() => { copyBtn.textContent = '⧉'; }, 1000);
        } catch (err) {
            console.error('Clipboard copy failed:', err);
        }
    };
    wrap.appendChild(copyBtn);

    const saveBtn = document.createElement('button');
    saveBtn.className = 'output-image-btn output-image-save';
    saveBtn.title = 'Save image';
    saveBtn.textContent = '⭳';
    saveBtn.onclick = (e) => {
        e.stopPropagation();
        const a = document.createElement('a');
        a.href = img.src;
        a.download = `jupyvim-plot-${Date.now()}.png`;
        document.body.appendChild(a);
        a.click();
        a.remove();
    };
    wrap.appendChild(saveBtn);

    return wrap;
}

function renderSavedOutput(container, out) {
    if (out.output_type === 'stream') {
        const pre = document.createElement('pre');
        pre.className = 'output-stream';
        pre.textContent = Array.isArray(out.text) ? out.text.join('') : (out.text || '');
        container.appendChild(pre);
    } else if (out.output_type === 'execute_result' || out.output_type === 'display_data') {
        if (out.data && out.data['image/png']) {
            container.appendChild(createImageOutput(out.data['image/png']));
        } else if (out.data && out.data['text/html']) {
            const htmlDiv = document.createElement('div');
            htmlDiv.className = 'output-html';
            htmlDiv.innerHTML = Array.isArray(out.data['text/html']) ? out.data['text/html'].join('') : out.data['text/html'];
            container.appendChild(htmlDiv);
        } else if (out.data && out.data['text/plain']) {
            const pre = document.createElement('pre');
            pre.className = 'output-stream';
            pre.textContent = Array.isArray(out.data['text/plain']) ? out.data['text/plain'].join('') : out.data['text/plain'];
            container.appendChild(pre);
        }
    } else if (out.output_type === 'error') {
        const errDiv = document.createElement('div');
        errDiv.className = 'output-error';
        const trace = out.traceback ? out.traceback.join('\n') : `${out.ename}: ${out.evalue}`;
        errDiv.textContent = trace.replace(/[][[()#;?]*(?:[0-9]{1,4}(?:;[0-9]{0,4})*)?[0-9A-ORZcf-nqry=><]/g, '');
        container.appendChild(errDiv);
    }
}

function clearAllOutputs() {
    cells.forEach((cell, idx) => {
        if (cell.cell_type === 'code') {
            const outEl = document.getElementById(`output-${idx}`);
            if (outEl) outEl.innerHTML = '';
            const execEl = document.querySelector(`#cell-${idx} .exec-count`);
            if (execEl) execEl.textContent = '[ ]';
        }
    });
}

// Execution: the browser only ever sends a cell index; the server holds the
// authoritative source (synced from Neovim), so there is no code to smuggle.
async function runCell(index) {
    const cell = cells[index];
    if (!cell || cell.cell_type !== 'code') return;

    const cellEl = document.getElementById(`cell-${index}`);
    const outputContainer = document.getElementById(`output-${index}`);
    if (cellEl) {
        cellEl.classList.remove('exec-success', 'exec-error');
        cellEl.classList.add('running');
    }
    if (outputContainer) outputContainer.innerHTML = '';

    // Registering the msg_id *before* sending avoids a race where fast
    // iopub messages (e.g. from a trivial cell) could otherwise arrive
    // before we knew to associate them with this cell.
    const msgId = 'jupyvim-' + Math.random().toString(36).slice(2, 12);
    cellExecMap.set(msgId, index);

    try {
        const res = await fetch('/api/execute_cell', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ index, msg_id: msgId }),
        });
        if (!res.ok) throw new Error('Execution request rejected by server');
    } catch (err) {
        console.error('Execution error:', err);
        if (cellEl) {
            cellEl.classList.remove('running');
            cellEl.classList.add('exec-error');
        }
        if (outputContainer) {
            const errDiv = document.createElement('div');
            errDiv.className = 'output-error';
            errDiv.textContent = 'Execution Error: ' + err.message;
            outputContainer.appendChild(errDiv);
        }
    }
}

// Runs every code cell strictly in order, waiting for the kernel to go idle
// before moving to the next one (via /api/execute_cell_wait, the same
// blocking endpoint Neovim's <leader>jx uses). Also drives a visible
// "Running N/M" progress indicator and scrolls the active cell into view,
// since silent cells (e.g. a block of imports) otherwise give no sign
// anything happened at all.
async function runAll() {
    const runAllBtn = document.getElementById('run-all-btn');
    const originalLabel = runAllBtn.textContent;
    const codeIndices = cells.map((c, i) => (c.cell_type === 'code' ? i : -1)).filter(i => i >= 0);

    for (let n = 0; n < codeIndices.length; n++) {
        const index = codeIndices[n];
        const cellEl = document.getElementById(`cell-${index}`);
        const outputContainer = document.getElementById(`output-${index}`);
        runAllBtn.textContent = `▶ Running ${n + 1}/${codeIndices.length}…`;

        if (cellEl) {
            cellEl.classList.remove('exec-success', 'exec-error');
            cellEl.classList.add('running');
            cellEl.scrollIntoView({ behavior: 'smooth', block: 'center' });
        }
        if (outputContainer) outputContainer.innerHTML = '';

        const msgId = 'jupyvim-' + Math.random().toString(36).slice(2, 12);
        cellExecMap.set(msgId, index);

        try {
            const res = await fetch('/api/execute_cell_wait', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ index, msg_id: msgId }),
            });
            const data = await res.json();
            if (cellEl) {
                cellEl.classList.remove('running');
                cellEl.classList.add(data.error ? 'exec-error' : 'exec-success');
            }
            if (data.error && outputContainer && !outputContainer.querySelector('.output-error')) {
                const errDiv = document.createElement('div');
                errDiv.className = 'output-error';
                errDiv.textContent = `${data.error.ename}: ${data.error.evalue}`;
                outputContainer.appendChild(errDiv);
            }
        } catch (err) {
            console.error('Run All error:', err);
            if (cellEl) {
                cellEl.classList.remove('running');
                cellEl.classList.add('exec-error');
            }
            break;
        }
    }

    runAllBtn.textContent = originalLabel;
}

// WebSocket Connection Management
function connectWebSocket() {
    const statusPill = document.getElementById('kernel-status');
    const statusText = document.getElementById('status-text');
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const wsUrl = `${proto}//${location.host}/ws`;

    ws = new WebSocket(wsUrl);

    ws.onopen = () => {
        statusPill.className = 'kernel-status connected';
        statusText.textContent = 'Python 3 (ipykernel)';
        console.log('[WebSocket] Connected to Jupyvim backend');
    };

    ws.onmessage = (event) => {
        try {
            const data = JSON.parse(event.data);
            handleServerMessage(data);
        } catch (e) {
            console.log('[WebSocket Raw]', event.data);
        }
    };

    ws.onclose = () => {
        statusPill.className = 'kernel-status disconnected';
        statusText.textContent = 'Disconnected';
        setTimeout(connectWebSocket, 2000);
    };

    ws.onerror = (err) => {
        console.error('[WebSocket Error]', err);
    };
}

function handleServerMessage(data) {
    if (data.type === 'notebook_update') {
        // Neovim saved the mirror file; re-fetch cells (structure + carried-over outputs).
        initNotebook();
        return;
    }
    if (data.type === 'iopub') {
        handleKernelMessage(data);
        return;
    }
    if (data.type === 'cursor_at_cell') {
        // Neovim -> browser cursor-follow: only fires when the cursor
        // crosses into a different cell (deduped in the Lua plugin), so
        // this is rare, not per-keystroke. The highlight is persistent
        // (moved, not flashed+timed-out) -- it's just a static CSS class
        // with no animation or timer, so keeping it up costs nothing extra;
        // it always shows exactly which cell Neovim's cursor is in.
        const cellEl = document.getElementById(`cell-${data.index}`);
        if (!cellEl) return;
        cellEl.scrollIntoView({ behavior: 'smooth', block: 'center' });
        document.querySelectorAll('.cell.cursor-follow').forEach(el => el.classList.remove('cursor-follow'));
        cellEl.classList.add('cursor-follow');
    }
}

function handleKernelMessage(data) {
    const { msg_type, parent_msg_id, content } = data;
    if (!parent_msg_id || !cellExecMap.has(parent_msg_id)) return;

    const cellIndex = cellExecMap.get(parent_msg_id);
    const cellEl = document.getElementById(`cell-${cellIndex}`);
    const outputContainer = document.getElementById(`output-${cellIndex}`);
    if (!outputContainer) return;

    if (msg_type === 'status') {
        const state = content ? content.execution_state : null;
        if (state === 'idle') {
            if (cellEl) {
                cellEl.classList.remove('running');
                // Silent cells (e.g. a block of imports) produce no output at
                // all, which otherwise looks identical to "nothing happened" --
                // the persistent ✓/✗ + duration is the real signal here.
                const hadError = outputContainer.querySelector('.output-error') != null;
                stopCellTimer(cellIndex, {
                    success: !hadError,
                    execCount: cellEl.dataset.pendingExecCount,
                });
            }
        } else if (state === 'busy') {
            if (cellEl) {
                cellEl.classList.add('running');
                delete cellEl.dataset.pendingExecCount;
                startCellTimer(cellIndex);
            }
        }
    } else if (msg_type === 'execute_input') {
        // Kept pending rather than shown immediately: VS Code keeps the
        // execution-count slot as "[*]" for the whole busy phase and only
        // reveals the real count once the cell actually finishes.
        const execCount = content ? content.execution_count : null;
        if (execCount != null && cellEl) {
            cellEl.dataset.pendingExecCount = execCount;
        }
    } else if (msg_type === 'stream') {
        let streamPre = outputContainer.querySelector('.output-stream');
        if (!streamPre) {
            streamPre = document.createElement('pre');
            streamPre.className = 'output-stream';
            outputContainer.appendChild(streamPre);
        }
        streamPre.textContent += (content.text || '');
    } else if (msg_type === 'execute_result' || msg_type === 'display_data') {
        if (content && content.data) {
            if (content.data['image/png']) {
                outputContainer.appendChild(createImageOutput(content.data['image/png']));
            } else if (content.data['text/html']) {
                const htmlDiv = document.createElement('div');
                htmlDiv.className = 'output-html';
                htmlDiv.innerHTML = content.data['text/html'];
                outputContainer.appendChild(htmlDiv);
            } else if (content.data['text/plain']) {
                const pre = document.createElement('pre');
                pre.className = 'output-stream';
                pre.textContent = content.data['text/plain'];
                outputContainer.appendChild(pre);
            }
        }
    } else if (msg_type === 'error') {
        const errDiv = document.createElement('div');
        errDiv.className = 'output-error';
        const trace = (content.traceback || []).join('\n') || `${content.ename}: ${content.evalue}`;
        errDiv.textContent = trace.replace(/[][[()#;?]*(?:[0-9]{1,4}(?:;[0-9]{0,4})*)?[0-9A-ORZcf-nqry=><]/g, '');
        outputContainer.appendChild(errDiv);
    }
}

// Bootstrapping
window.addEventListener('DOMContentLoaded', () => {
    initNotebook();
    connectWebSocket();

    document.getElementById('run-all-btn').onclick = runAll;
    document.getElementById('clear-outputs-btn').onclick = clearAllOutputs;
});
