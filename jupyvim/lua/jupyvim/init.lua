local M = {}

local job_id = nil
local server_ready = false
local current_ipynb = nil
local mirror_path = nil
local mirror_bufnr = nil
local server_port = 3000
local ready_callbacks = {}

local ns = vim.api.nvim_create_namespace("jupyvim_cell_status")
-- "bufnr:marker_line" -> extmark id, so each cell keeps its own persistent
-- status (live timer while running, ✓/✗ + duration once done) instead of
-- wiping every other cell's status whenever a new one starts running.
local status_marks = {}

-- Root of this plugin's own checkout (wherever it was cloned/installed),
-- derived from this file's own location rather than hardcoded, so the
-- plugin works from any path on any machine.
local plugin_root = vim.fn.fnamemodify(debug.getinfo(1, "S").source:sub(2), ":p:h:h:h")

-- Debug log lives next to the plugin checkout (gitignored).
local log_file_path = plugin_root .. "/jupyvim_debug.log"

local function log(level, message)
    local fp = io.open(log_file_path, "a")
    if fp then
        local timestamp = os.date("%Y-%m-%d %H:%M:%S")
        fp:write(string.format("[%s] [%s] %s\n", timestamp, level, message))
        fp:close()
    end
end

-- Locates the Jupyvim Rust binary (prioritizing release build)
local function find_binary()
    local candidates = {
        plugin_root .. "/target/release/jupyvim",
        plugin_root .. "/target/debug/jupyvim",
    }
    for _, path in ipairs(candidates) do
        if vim.fn.executable(path) == 1 then
            return path
        end
    end
    if vim.fn.executable("jupyvim") == 1 then
        return "jupyvim"
    end
    return candidates[1]
end

-- Determines project working directory for the given notebook
local function get_project_dir(filepath)
    if not filepath or filepath == "" then
        return vim.fn.getcwd()
    end
    local dir = vim.fn.fnamemodify(filepath, ":p:h")
    if dir and dir ~= "" and vim.fn.isdirectory(dir) == 1 then
        return dir
    end
    return vim.fn.getcwd()
end

local function base_url()
    return "http://127.0.0.1:" .. server_port
end

-- Opens web browser with Jupyvim preview URL
local function open_browser(url)
    url = url or base_url()
    local open_cmd = nil
    if vim.fn.has("mac") == 1 then
        open_cmd = "open"
    elseif vim.fn.has("unix") == 1 then
        open_cmd = "xdg-open"
    else
        log("WARN", "No suitable browser open command found for this operating system.")
        print("[Jupyvim] Could not open browser automatically. Please open: " .. url)
        return
    end

    vim.fn.jobstart({ open_cmd, url }, { detach = true })
    log("INFO", "Browser opened for URL: " .. url)
    print("==> [Jupyvim] Browser preview opened: " .. url)
end

-- Starts the Rust backend daemon (jupyvim)
local function start_backend(filename, on_ready)
    if on_ready then
        table.insert(ready_callbacks, on_ready)
    end

    if job_id then
        log("INFO", "Backend is already running (job_id: " .. tostring(job_id) .. ")")
        if server_ready then
            for _, cb in ipairs(ready_callbacks) do
                cb()
            end
            ready_callbacks = {}
        end
        return
    end

    local bin_path = find_binary()
    if vim.fn.executable(bin_path) ~= 1 then
        local err_msg = "Jupyvim binary not found or not executable: " .. bin_path
        log("ERROR", err_msg)
        vim.api.nvim_echo({{ "[Jupyvim Error] " .. err_msg, "ErrorMsg" }}, true, {})
        return
    end

    current_ipynb = filename
    local cwd = get_project_dir(filename)
    server_ready = false

    log("INFO", "==================================================")
    log("INFO", "Starting backend for file: " .. tostring(filename))
    log("INFO", "Binary: " .. bin_path)
    log("INFO", "CWD: " .. cwd)

    local cmd = { bin_path, "--file", filename, "--port", tostring(server_port) }
    if vim.v.servername and vim.v.servername ~= "" then
        table.insert(cmd, "--nvim-server")
        table.insert(cmd, vim.v.servername)
    end

    job_id = vim.fn.jobstart(cmd, {
        cwd = cwd,
        on_stdout = function(_, data)
            if not data then return end
            for _, line in ipairs(data) do
                if line ~= "" then
                    log("STDOUT", line)
                    if line:find("Axum server on") or line:find("Starte Axum%-Server") then
                        server_ready = true
                        print("[Jupyvim] Backend & server active at " .. base_url())
                        for _, cb in ipairs(ready_callbacks) do
                            cb()
                        end
                        ready_callbacks = {}
                    end
                end
            end
        end,
        on_stderr = function(_, data)
            if not data then return end
            for _, line in ipairs(data) do
                if line ~= "" then
                    log("STDERR", line)
                    if line:find("Error") or line:find("panic") or line:find("Fehler") then
                        vim.api.nvim_echo({{ "[Jupyvim Error] " .. line, "ErrorMsg" }}, false, {})
                    end
                end
            end
        end,
        on_exit = function(_, code)
            log("INFO", "Backend terminated with exit code: " .. tostring(code))
            print("[Jupyvim] Backend terminated (code: " .. tostring(code) .. ")")
            job_id = nil
            server_ready = false
            ready_callbacks = {}
        end,
    })

    if job_id <= 0 then
        local err_msg = "jobstart failed with return code: " .. tostring(job_id)
        log("ERROR", err_msg)
        vim.api.nvim_echo({{ "[Jupyvim Error] " .. err_msg, "ErrorMsg" }}, true, {})
        job_id = nil
    else
        log("INFO", "Backend started successfully with job_id: " .. tostring(job_id))
    end
end

-- Stops the backend daemon
local function stop_backend()
    if job_id then
        log("INFO", "Stopping backend (job_id: " .. tostring(job_id) .. ")")
        vim.fn.jobstop(job_id)
        job_id = nil
        server_ready = false
        ready_callbacks = {}
    end
end

-- Fetches the path of the Jupytext "percent"-format mirror file that the
-- Rust backend generated from the .ipynb, then opens *that* file in the
-- current window (replacing the raw-JSON buffer). This is what gives us
-- real Python syntax, tree-sitter and LSP for free: we never edit .ipynb
-- JSON directly, we edit a plain Python script that Rust keeps in sync.
local function open_mirror_buffer(ipynb_buf)
    if vim.fn.executable("curl") ~= 1 then
        vim.api.nvim_echo({{ "[Jupyvim Error] curl is required but not found in PATH.", "ErrorMsg" }}, true, {})
        return
    end

    local result = vim.fn.system({ "curl", "-s", base_url() .. "/api/mirror" })
    if vim.v.shell_error ~= 0 then
        log("ERROR", "Failed to fetch mirror path: " .. tostring(result))
        vim.api.nvim_echo({{ "[Jupyvim Error] Could not reach backend to fetch mirror file.", "ErrorMsg" }}, true, {})
        return
    end

    local ok, decoded = pcall(vim.fn.json_decode, result)
    if not ok or not decoded or not decoded.mirror_path then
        log("ERROR", "Failed to decode /api/mirror response: " .. tostring(result))
        return
    end

    mirror_path = decoded.mirror_path
    log("INFO", "Opening mirror buffer: " .. mirror_path)

    vim.cmd("edit " .. vim.fn.fnameescape(mirror_path))
    vim.bo.filetype = "python"

    if ipynb_buf and vim.api.nvim_buf_is_valid(ipynb_buf) and ipynb_buf ~= vim.api.nvim_get_current_buf() then
        pcall(vim.api.nvim_buf_delete, ipynb_buf, { force = true })
    end

    M.setup_mirror_buffer(vim.api.nvim_get_current_buf())
end

-- Starts a backend session directly against a .py file (no hidden mirror
-- copy needed: the file the user is already editing IS the mirror). Used
-- both for auto-detected Jupytext-percent scripts and for bootstrapping a
-- brand new notebook out of a plain script via :JupyvimNewNotebook.
local function start_py_notebook(buf, filepath)
    start_backend(filepath, function()
        mirror_path = filepath
        M.setup_mirror_buffer(buf)
        vim.api.nvim_echo({{ "[Jupyvim] Notebook powers enabled for " .. vim.fn.fnamemodify(filepath, ":t")
            .. " — save to sync, <leader>jx to run a cell.", "Normal" }}, false, {})
    end)
end

-- Auto-detects a Jupytext "percent" formatted script (first line starts with
-- "# %%", the same convention VS Code / Spyder use for interactive cells)
-- and transparently wires it up to a backend session.
local function handle_py_buffer(buf)
    if not vim.api.nvim_buf_is_valid(buf) then return end
    if job_id then return end -- a backend session is already active
    local filepath = vim.api.nvim_buf_get_name(buf)
    if not filepath:match("%.py$") then return end

    local first_line = vim.api.nvim_buf_get_lines(buf, 0, 1, false)[1] or ""
    if first_line:match("^# %%%%") then
        log("INFO", "Auto-detected Jupytext percent script: " .. filepath)
        start_py_notebook(buf, filepath)
    end
end

-- Sends the mirror buffer's full content to the backend so it can re-parse
-- cell structure and notify the browser preview. `persist` (true on `:w`)
-- also writes the real .ipynb to disk; live-typing calls pass false so
-- keystrokes only ever update memory + the browser, never the filesystem.
local function sync_buffer(buf, persist)
    if not server_ready then return end
    local lines = vim.api.nvim_buf_get_lines(buf, 0, -1, false)
    local content = table.concat(lines, "\n") .. "\n"

    local job = vim.fn.jobstart({ "curl", "-s", "-X", "POST", base_url() .. "/api/sync",
        "-H", "Content-Type: application/json",
        "-d", "@-",
    }, { stdin = "pipe" })
    vim.fn.chansend(job, vim.fn.json_encode({ content = content, persist = persist ~= false }))
    vim.fn.chanclose(job, "stdin")
end

-- Debounced live-preview sync: TextChanged/TextChangedI fire on every single
-- edit, which would otherwise mean spawning a curl process per keystroke.
-- Instead each edit just resets a per-buffer 120ms timer, so a burst of fast
-- typing collapses into exactly one sync shortly after you pause -- no disk
-- I/O involved at all (persist=false), just an in-memory reparse + a
-- WebSocket push to the browser.
local live_sync_timers = {}

local function live_sync_buffer(buf)
    local existing = live_sync_timers[buf]
    if existing then
        existing:stop()
    else
        live_sync_timers[buf] = vim.uv.new_timer()
    end
    live_sync_timers[buf]:start(120, 0, vim.schedule_wrap(function()
        if vim.api.nvim_buf_is_valid(buf) then
            sync_buffer(buf, false)
        end
    end))
end

-- Counts "# %%" cell markers to determine which cell index the cursor is in,
-- and the marker's line number (for placing status virtual text).
local function cell_at_cursor(buf)
    local cursor_line = vim.api.nvim_win_get_cursor(0)[1]
    local lines = vim.api.nvim_buf_get_lines(buf, 0, cursor_line, false)
    local index = -1
    local marker_line = 1
    for i, line in ipairs(lines) do
        if line:match("^# %%%%") then
            index = index + 1
            marker_line = i
        end
    end
    if index < 0 then index = 0 end
    return index, marker_line
end

-- Neovim -> browser cursor-follow. CursorMoved fires on *every* cursor
-- step, so this only ever does work when the computed cell index actually
-- changes (crossing a cell boundary) -- moving within a cell is free. A
-- short debounce on top of that means holding `j`/`k` through several
-- cells collapses into one request, not one per line.
local cursor_follow_timers = {}
local last_notified_cell = {}

local function notify_cursor_moved(buf)
    local index = cell_at_cursor(buf)
    if last_notified_cell[buf] == index then return end

    local existing = cursor_follow_timers[buf]
    if existing then
        existing:stop()
    else
        cursor_follow_timers[buf] = vim.uv.new_timer()
    end
    cursor_follow_timers[buf]:start(80, 0, vim.schedule_wrap(function()
        if not (server_ready and vim.api.nvim_buf_is_valid(buf)) then return end
        last_notified_cell[buf] = index
        vim.fn.jobstart({ "curl", "-s", "-X", "POST", base_url() .. "/api/cursor_moved",
            "-H", "Content-Type: application/json",
            "-d", vim.fn.json_encode({ index = index }),
        })
    end))
end

-- Creates or updates (in place, by extmark id) the status virtual-text for
-- one specific cell, so setting one cell's status never disturbs another
-- cell's already-finished ✓/✗ marker elsewhere in the buffer.
local function set_cell_status(buf, line, text, hl)
    local key = buf .. ":" .. line
    local id = vim.api.nvim_buf_set_extmark(buf, ns, line - 1, 0, {
        id = status_marks[key],
        virt_text = { { text, hl or "Comment" } },
        virt_text_pos = "eol",
    })
    status_marks[key] = id
end

-- Executes the cell under the cursor via /api/execute_cell_wait. Mirrors
-- VS Code's three-phase notebook status: a live "N.Ns" timer while the
-- request blocks server-side waiting for the kernel to go idle, then a
-- persistent ✓/✗ + final duration once the async job's on_exit fires
-- (persistent, not auto-cleared -- matches the browser's gutter indicator).
local function execute_cell_at_cursor()
    if not server_ready then
        vim.api.nvim_echo({{ "[Jupyvim] Backend is not ready yet.", "WarningMsg" }}, true, {})
        return
    end
    local buf = vim.api.nvim_get_current_buf()
    local index, marker_line = cell_at_cursor(buf)

    local start_ns = vim.uv.hrtime()
    set_cell_status(buf, marker_line, " ⏳ 0.0s", "DiagnosticWarn")

    local timer = vim.uv.new_timer()
    timer:start(100, 100, vim.schedule_wrap(function()
        if not vim.api.nvim_buf_is_valid(buf) then
            timer:stop()
            timer:close()
            return
        end
        local elapsed = (vim.uv.hrtime() - start_ns) / 1e9
        set_cell_status(buf, marker_line, string.format(" ⏳ %.1fs", elapsed), "DiagnosticWarn")
    end))

    local function finish(text, hl)
        timer:stop()
        timer:close()
        if vim.api.nvim_buf_is_valid(buf) then
            set_cell_status(buf, marker_line, text, hl)
        end
    end

    local function elapsed_str()
        return string.format("%.1fs", (vim.uv.hrtime() - start_ns) / 1e9)
    end

    local response_chunks = {}
    local job = vim.fn.jobstart({ "curl", "-s", "-X", "POST", base_url() .. "/api/execute_cell_wait",
        "-H", "Content-Type: application/json",
        "-d", vim.fn.json_encode({ index = index }),
    }, {
        stdout_buffered = true,
        on_stdout = function(_, data)
            if data then
                for _, line in ipairs(data) do
                    table.insert(response_chunks, line)
                end
            end
        end,
        on_exit = function(_, code)
            if code ~= 0 then
                finish(" ✘ Could not reach backend", "DiagnosticError")
                return
            end

            local ok, decoded = pcall(vim.fn.json_decode, table.concat(response_chunks, "\n"))
            if not ok or not decoded then
                finish(" ✘ Malformed response from backend", "DiagnosticError")
                return
            end

            if decoded.status == "error" then
                finish(" ✘ Cell could not be executed (not a code cell?)", "DiagnosticError")
                return
            end

            if decoded.timed_out then
                finish(string.format(" ⏱ Timed out after %s (still running)", elapsed_str()), "DiagnosticWarn")
                return
            end

            if decoded.error then
                local ename = decoded.error.ename or "Error"
                local evalue = decoded.error.evalue or ""
                finish(string.format(" ✘ %s — %s: %s", elapsed_str(), ename, evalue), "DiagnosticError")
                log("ERROR", string.format("Cell %d failed: %s: %s", index, ename, evalue))
                return
            end

            finish(string.format(" ✔ %s", elapsed_str()), "DiagnosticOk")
        end,
    })
    if job <= 0 then
        finish(" ✘ Could not reach backend", "DiagnosticError")
    end
end

-- Sets up buffer-local sync-on-save, live-preview sync while typing, and
-- the cell execution keybinding for a mirror .py buffer.
function M.setup_mirror_buffer(buf)
    mirror_bufnr = buf
    local group = vim.api.nvim_create_augroup("JupyvimMirrorSync_" .. buf, { clear = true })
    vim.api.nvim_create_autocmd("BufWritePost", {
        group = group,
        buffer = buf,
        callback = function()
            local timer = live_sync_timers[buf]
            if timer then timer:stop() end -- a real save supersedes any pending live-preview sync
            sync_buffer(buf, true)
        end,
    })

    -- Live preview: no <leader> needed, no :w needed -- see live_sync_buffer.
    vim.api.nvim_create_autocmd({ "TextChanged", "TextChangedI" }, {
        group = group,
        buffer = buf,
        callback = function()
            live_sync_buffer(buf)
        end,
    })

    -- Cursor-follow: browser scrolls/highlights the cell your cursor is in.
    vim.api.nvim_create_autocmd({ "CursorMoved", "CursorMovedI" }, {
        group = group,
        buffer = buf,
        callback = function()
            notify_cursor_moved(buf)
        end,
    })

    vim.api.nvim_create_autocmd({ "BufWipeout", "BufDelete" }, {
        group = group,
        buffer = buf,
        callback = function()
            local timer = live_sync_timers[buf]
            if timer then
                timer:stop()
                timer:close()
                live_sync_timers[buf] = nil
            end
            local cf_timer = cursor_follow_timers[buf]
            if cf_timer then
                cf_timer:stop()
                cf_timer:close()
                cursor_follow_timers[buf] = nil
            end
            last_notified_cell[buf] = nil
        end,
    })

    vim.keymap.set("n", "<leader>jx", execute_cell_at_cursor,
        { buffer = buf, silent = true, desc = "Jupyvim: Run Cell Under Cursor" })
    vim.keymap.set("i", "<S-CR>", function()
        vim.cmd("stopinsert")
        execute_cell_at_cursor()
    end, { buffer = buf, silent = true, desc = "Jupyvim: Run Cell Under Cursor" })
    vim.keymap.set("n", "<S-CR>", execute_cell_at_cursor,
        { buffer = buf, silent = true, desc = "Jupyvim: Run Cell Under Cursor" })
end

-- Processes a notebook buffer (starts backend, then swaps in the mirror .py)
local function handle_ipynb_buffer(buf)
    if not vim.api.nvim_buf_is_valid(buf) then return end
    local filepath = vim.api.nvim_buf_get_name(buf)
    if not filepath:match("%.ipynb$") then return end

    log("INFO", "Processing ipynb buffer: " .. filepath)
    start_backend(filepath, function()
        open_mirror_buffer(buf)
    end)
end

function M.setup()
    log("INFO", "Initializing Jupyvim plugin (setup)...")

    local group = vim.api.nvim_create_augroup("JupyvimAutocmds", { clear = true })

    -- Autocmd for notebook files opened after setup
    vim.api.nvim_create_autocmd({ "BufReadPost", "BufNewFile" }, {
        group = group,
        pattern = "*.ipynb",
        callback = function(args)
            handle_ipynb_buffer(args.buf)
        end,
    })

    -- Autocmd for plain .py scripts that already use the Jupytext "percent"
    -- cell convention (# %%) -- wires them up automatically, no .ipynb needed.
    vim.api.nvim_create_autocmd({ "BufReadPost", "BufNewFile" }, {
        group = group,
        pattern = "*.py",
        callback = function(args)
            handle_py_buffer(args.buf)
        end,
    })

    -- Handle existing buffer if plugin was loaded lazily on FileType
    local current_buf = vim.api.nvim_get_current_buf()
    local cur_name = vim.api.nvim_buf_get_name(current_buf)
    if cur_name:match("%.ipynb$") then
        log("INFO", "Current buffer is already ipynb. Triggering handle_ipynb_buffer directly.")
        handle_ipynb_buffer(current_buf)
    end

    -- Clean shutdown on Neovim exit (battery protection)
    vim.api.nvim_create_autocmd("VimLeavePre", {
        group = group,
        callback = function()
            stop_backend()
        end,
    })

    -- User Command: Add Code Cell (Jupytext "percent" marker)
    vim.api.nvim_create_user_command("JupyvimAddCodeCell", function()
        local line = vim.api.nvim_win_get_cursor(0)[1]
        vim.api.nvim_buf_set_lines(0, line, line, false, { "", "# %%", "" })
        vim.api.nvim_win_set_cursor(0, { line + 3, 0 })
    end, { desc = "Inserts a Jupyter code cell marker" })

    -- User Command: Add Markdown Cell (Jupytext "percent" marker)
    vim.api.nvim_create_user_command("JupyvimAddMarkdownCell", function()
        local line = vim.api.nvim_win_get_cursor(0)[1]
        vim.api.nvim_buf_set_lines(0, line, line, false, { "", "# %% [markdown]", "# " })
        vim.api.nvim_win_set_cursor(0, { line + 3, 2 })
    end, { desc = "Inserts a Jupyter markdown cell marker" })

    -- User Command: Bootstrap the current plain .py script into a
    -- Jupyter-backed notebook (no # %% markers required -- the whole file
    -- becomes a single leading code cell you can then split up).
    vim.api.nvim_create_user_command("JupyvimNewNotebook", function()
        local buf = vim.api.nvim_get_current_buf()
        local filepath = vim.api.nvim_buf_get_name(buf)
        if not filepath:match("%.py$") then
            vim.api.nvim_echo({{ "[Jupyvim] Current buffer is not a .py file.", "WarningMsg" }}, true, {})
            return
        end
        if job_id then
            vim.api.nvim_echo({{ "[Jupyvim] A backend session is already active in this Neovim instance.", "WarningMsg" }}, true, {})
            return
        end
        if vim.bo[buf].modified or vim.fn.filereadable(filepath) == 0 then
            vim.cmd("write")
        end
        start_py_notebook(buf, filepath)
    end, { desc = "Bootstraps the current .py script into a Jupyter-backed notebook" })

    -- User Command: Run cell under cursor
    vim.api.nvim_create_user_command("JupyvimRunCell", execute_cell_at_cursor,
        { desc = "Runs the Jupyter cell under the cursor" })

    -- User Command: Open on-demand browser preview
    vim.api.nvim_create_user_command("JupyvimPreview", function()
        local filepath = current_ipynb or vim.fn.expand("%:p")

        if not filepath:match("%.ipynb$") then
            vim.api.nvim_echo({{ "[Jupyvim] No active .ipynb notebook for this session.", "WarningMsg" }}, true, {})
            return
        end

        if not job_id then
            print("[Jupyvim] Starting backend on-demand...")
            start_backend(filepath, function()
                open_browser(base_url())
            end)
        elseif not server_ready then
            print("[Jupyvim] Backend starting, opening browser once ready...")
            table.insert(ready_callbacks, function()
                open_browser(base_url())
            end)
        else
            open_browser(base_url())
        end
    end, { desc = "Starts backend on-demand and opens browser preview" })

    -- User Command: Show backend status
    vim.api.nvim_create_user_command("JupyvimStatus", function()
        local status = {
            "== Jupyvim Status ==",
            "Backend Job ID: " .. (job_id and tostring(job_id) or "Inactive"),
            "Server Ready:   " .. (server_ready and ("Yes (" .. base_url() .. ")") or "No"),
            "Active Notebook:" .. (current_ipynb or "None"),
            "Mirror File:    " .. (mirror_path or "None"),
            "Log File:       " .. log_file_path,
        }
        vim.api.nvim_echo(vim.tbl_map(function(l) return { l .. "\n", "Normal" } end, status), true, {})
    end, { desc = "Displays current status of Jupyvim backend" })

    -- User Command: Open debug log in a split window
    vim.api.nvim_create_user_command("JupyvimLog", function()
        vim.cmd("split " .. vim.fn.fnameescape(log_file_path))
    end, { desc = "Opens Jupyvim debug log in a split window" })

    -- User Command: Restart backend
    vim.api.nvim_create_user_command("JupyvimRestart", function()
        local filepath = current_ipynb
        stop_backend()
        vim.defer_fn(function()
            if filepath then
                start_backend(filepath, function()
                    vim.api.nvim_echo({{ "[Jupyvim] Backend restarted.", "Normal" }}, false, {})
                end)
            end
        end, 500)
    end, { desc = "Restarts Jupyvim backend" })

    -- Configure keymaps
    vim.keymap.set("n", "<leader>jn", ":JupyvimNewNotebook<CR>", { silent = true, desc = "Jupyvim: Bootstrap Notebook From Script" })
    vim.keymap.set("n", "<leader>jc", ":JupyvimAddCodeCell<CR>", { silent = true, desc = "Jupyvim: Add Code Cell" })
    vim.keymap.set("n", "<leader>jm", ":JupyvimAddMarkdownCell<CR>", { silent = true, desc = "Jupyvim: Add Markdown Cell" })
    vim.keymap.set("n", "<leader>jp", ":JupyvimPreview<CR>", { silent = true, desc = "Jupyvim: Open Browser Preview" })
    vim.keymap.set("n", "<leader>js", ":JupyvimStatus<CR>", { silent = true, desc = "Jupyvim: Show Status" })
    vim.keymap.set("n", "<leader>jl", ":JupyvimLog<CR>", { silent = true, desc = "Jupyvim: View Debug Log" })
end

-- Exported functions for Lua invocation
M.start_backend = start_backend
M.stop_backend = stop_backend
M.status = function() vim.cmd("JupyvimStatus") end

-- Browser -> Neovim click-to-jump entry point. Called via
-- `nvim --server <addr> --remote-expr "v:lua.require('jupyvim').jump_to_line(N)"`
-- from the Rust backend (see server.rs / jump_neovim_to_line). Finds the
-- window actually showing the mirror buffer (a remote-expr call doesn't run
-- in any particular window), focuses it, and centers the cursor on the line.
function M.jump_to_line(line)
    if not (mirror_bufnr and vim.api.nvim_buf_is_valid(mirror_bufnr)) then
        return 0
    end
    local winid = vim.fn.bufwinid(mirror_bufnr)
    if winid == -1 then
        -- Mirror buffer isn't visible in any window of this tab; open it
        -- in the current window rather than silently doing nothing.
        vim.api.nvim_set_current_buf(mirror_bufnr)
        winid = vim.api.nvim_get_current_win()
    end
    local line_count = vim.api.nvim_buf_line_count(mirror_bufnr)
    local target = math.max(1, math.min(line, line_count))
    vim.api.nvim_set_current_win(winid)
    vim.api.nvim_win_set_cursor(winid, { target, 0 })
    vim.api.nvim_win_call(winid, function()
        vim.cmd("normal! zz")
    end)
    return 1
end

return M
