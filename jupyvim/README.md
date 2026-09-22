# Jupyvim

A high-performance Jupyter backend for Neovim, written in Rust. It lets you write and run Jupyter notebooks entirely inside Neovim — with real Python syntax highlighting, tree-sitter, and LSP (Pyright, Ruff, …) — while a lightweight browser tab renders the live, VS-Code-style output preview (plots, DataFrames, rich outputs).

## Requirements

- **Rust** (stable toolchain, `cargo`) — to build the daemon.
- **Neovim** ≥ 0.10 (uses `vim.uv`, `vim.fn.jobstart`, and the `--remote-expr` server protocol).
- **`curl`** in `$PATH` — the Lua plugin shells out to it for all backend requests.
- **Python** with `ipykernel` installed in the environment you want notebooks to run against (a `uv`-managed `.venv` is auto-detected, see below).
- A plugin manager for Neovim (`lazy.nvim`, `packer`, or just `:set rtp+=`) — Jupyvim ships as a normal Neovim plugin with a Lua module.

## Installation

1. **Clone and build the Rust daemon:**

   ```bash
   git clone https://github.com/sudo-Oliver/Jupyvim.git
   cd Jupyvim/jupyvim
   cargo build --release
   ```

   This produces `target/release/jupyvim`. The Lua plugin looks for this binary relative to its own checkout automatically (falling back to `target/debug/jupyvim`, then a `jupyvim` on `$PATH`) — no manual `PATH` setup or config needed as long as you keep the Rust project and the `lua/` plugin together in the same checkout, which is what happens by default when you point your plugin manager straight at this repo.

2. **Add the plugin to Neovim.** With `lazy.nvim`:

   ```lua
   {
     "sudo-Oliver/Jupyvim",
     dir = "~/path/to/Jupyvim/jupyvim", -- point at your local clone (see step 1)
     build = "cargo build --release",
     config = function()
       require("jupyvim").setup()
     end,
   }
   ```

   `build` re-runs `cargo build --release` whenever the plugin updates, so the binary stays in sync with the Lua side. With `packer.nvim`, use `run = "cargo build --release"` instead of `build`.

   Jupyvim isn't published for direct install by plugin name yet (the Rust daemon and the Lua plugin live in the same repo, under `jupyvim/`, not at the repo root) — clone it yourself and point your plugin manager at the local checkout with `dir`/`config.path`, as above.

3. **Make sure a kernel is available.** In the project/notebook directory:

   ```bash
   uv init          # if you don't have a venv yet
   uv add ipykernel
   ```

   Jupyvim walks up from the notebook's directory looking for `.venv/bin/python`, so any `uv`-managed environment "just works." Without `uv`, point it at a specific interpreter instead — see [On the Astral stack](#on-the-astral-stack-uv-ruff-ty) below.

4. **Open a notebook:**

   ```bash
   nvim my_notebook.ipynb
   ```

   Jupyvim starts the backend, spawns a kernel, and swaps in the editable Python mirror automatically. Run `<leader>jp` to open the live browser preview, and `<leader>jx` / `Shift+Enter` to execute the cell under your cursor. See [Keybindings & commands](#keybindings--commands) for the full list.

Starting from a blank script instead of an `.ipynb`? See [workflow 2](#2-starting-a-notebook-from-scratch-eg-from-a-uv-init-script) — write a normal `.py` file and run `:JupyvimNewNotebook` on it.

## Why

Neovim is great for writing Python files. It is terrible at editing `.ipynb` files, because they are JSON on disk — there is nothing for a text editor to sensibly syntax-highlight or complete. Jupyvim's answer, inspired by [Tinymist](https://github.com/Myriad-Dreamin/tinymist) (Typst's LSP + live-preview model): **never edit the JSON**. You always edit a plain Python script; Jupyvim keeps a real `.ipynb` in sync behind the scenes and renders outputs in the browser.

## Architecture

```
Neovim (.py mirror, real Python) --BufWritePost--> Rust daemon --ZMQ--> ipykernel
        ^                                              |
        | <leader>jx (execute_cell_wait)                v
        |                                          .ipynb on disk
        |                                              |
        +-------------------- Browser (read-only, live via WebSocket) <-+
```

- **Rust daemon** (`src/`): one process per open notebook. Spawns a real `ipykernel` subprocess and talks to it over ZeroMQ (shell + iopub sockets, HMAC-signed, exactly like Jupyter itself). Exposes an Axum HTTP/WebSocket server.
- **The `.ipynb` file is always the ground truth.** It's what gets persisted, what you'd hand to a professor or push to GitHub.
- **The mirror file** (a plain `.py` using the [Jupytext "percent" format](https://jupytext.readthedocs.io/en/latest/formats-scripts.html#the-percent-format): `# %%` separates code cells, `# %% [markdown]` separates markdown cells) is what Neovim actually opens and edits. It is generated from the `.ipynb` and regenerated on every backend start.
- **The browser** is a read-only preview. It never edits cell content or structure — that's Neovim's job now. It renders server-side syntax-highlighted code (via `syntect`) and rendered markdown (via `pulldown-cmark`), shows live kernel output over a WebSocket, and its ▶ buttons call the same `execute_cell` API that Neovim's keybinding uses.

Whichever side triggers execution — a Neovim keybinding or a browser click — always executes the source of truth held by the Rust server (never client-supplied code), so both surfaces can never drift into executing different code for the same cell.

## Cell format (Jupytext "percent")

```python
# %% [markdown]
# # My Notebook
#
# Some explanation text.

# %%
import numpy as np
x = np.arange(10)
print(x)
```

This is a real, widely-used convention (VS Code's Python Interactive window and Spyder use the same `# %%` marker for cells), so files Jupyvim produces are readable and runnable outside Jupyvim too.

## Workflows

### 1. Opening an existing `.ipynb`

Open any `*.ipynb` in Neovim as usual. Jupyvim:
1. Starts the Rust daemon for that file (spawning a kernel, using the nearest `.venv/bin/python` found by walking up from the notebook's directory — this is exactly where `uv venv` / `uv init` put theirs, so **`uv`-managed environments just work, no extra config**).
2. Converts the notebook into a Jupytext-percent mirror at `.jupyvim/<name>.py` (a hidden directory next to the notebook).
3. Swaps the buffer to that mirror file and deletes the raw-JSON buffer. You now have a normal `.py` buffer: full tree-sitter highlighting, full LSP (Pyright/Ruff/`ty`/whatever you have configured — Jupyvim does nothing special here, it's just a `.py` file).
4. On every `:w`, the mirror's content is POSTed to the backend (`persist: true`), which re-parses cell structure, re-writes the `.ipynb`, and tells the browser to refresh.
5. **Live preview while typing, no `:w` needed:** `TextChanged`/`TextChangedI` also sync the buffer, debounced to a single POST ~120ms after you pause (`persist: false`) — this updates the in-memory notebook and pushes it to the browser over the WebSocket, but never touches disk. A burst of fast typing collapses into one sync, not one per keystroke. The browser patches only the changed cell's content in place rather than re-rendering the whole notebook, so it doesn't flicker or disturb another cell's in-flight output. Markdown renders live since it can't ever "fail". Code cells' *text* updates live too, but the kernel only ever runs code on an explicit `<leader>jx` / ▶ click — typing never executes anything, so there's no risk of syntax-error spam from half-finished code.
6. **Bidirectional cursor sync, no extra dependency:**
   - **Browser → Neovim:** every cell has a small "↦ code" button. Clicking it sends `{event: "jump_to_line", line}` over the already-open WebSocket; the Rust backend shells out to `nvim --server <addr> --remote-expr "v:lua.require('jupyvim').jump_to_line(N)"` — Neovim's own built-in remote-control protocol, so there's no msgpack-RPC client dependency in Rust at all. `jump_to_line` (in `init.lua`) finds the window showing the mirror buffer, focuses it, and centers the cursor (`zz`). The backend is told which Neovim to target via `--nvim-server $v:servername`, passed automatically when the plugin starts it.
   - **Neovim → browser:** `CursorMoved`/`CursorMovedI` recompute which cell the cursor is in; a request only fires when that index actually *changes* (crossing a cell boundary — moving within a cell costs nothing), debounced 80ms on top so holding `j`/`k` through several cells collapses into one request. The browser smooth-scrolls to and briefly highlights the matching cell.
   - Both directions are pure event-driven pushes over connections that are already open (the WebSocket, and one-off `nvim --server` subprocess calls whose children are reaped automatically) — no polling loop anywhere, which matters for multi-hour on-battery use.

Don't hand-edit the hidden `.jupyvim/*.py` mirror directly in a separate session — always open the `.ipynb`, which is what regenerates and owns that mirror.

### 2. Starting a notebook from scratch (e.g. from a `uv init` script)

You don't have to start from an `.ipynb`. Two ways in:

- **Explicit:** write a normal `.py` file however you like (`uv init`, blank file, whatever). When you're ready to add notebook powers, run `:JupyvimNewNotebook` (or `<leader>jn`) on that buffer. The whole file becomes a single leading code cell in a freshly created sibling `.ipynb`; split it into more cells afterwards with `<leader>jc` / `<leader>jm`. The `.py` file you were already editing *is* the mirror from that point on — no buffer swap, no hidden copy.
- **Automatic:** if a `.py` file's first line is already `# %%` (you wrote it that way, or it's a Jupytext file from elsewhere), Jupyvim detects that on open and wires it up automatically — no command needed.

### 3. Importing notebooks from Kaggle, classmates, professors, etc.

Any standard nbformat-v4 `.ipynb` opens the same way as workflow 1. Existing outputs (stdout streams, `text/plain`, `text/html` — e.g. pandas DataFrame tables, `image/png` plots) and existing `execution_count`s are preserved and shown in the browser preview untouched. `raw` cells round-trip correctly (`# %% [raw]` marker) and are rejected if you try to execute them (they aren't code). Markdown and code cells convert losslessly both directions — this has a round-trip unit test (`cargo test`) covering exactly that.

One caveat: notebooks written by the *actual* `jupytext` tool sometimes include a YAML front-matter header (`# ---\n# jupyter:\n# ...`) in their percent-format files. Jupyvim doesn't parse that header specially — it'll show up as inert leading comment text in the first cell rather than being stripped. Doesn't break anything, just isn't pretty.

## Keybindings & commands

| Keybinding | Command | Effect |
|---|---|---|
| `<leader>jn` | `:JupyvimNewNotebook` | Bootstrap the current `.py` buffer into a notebook |
| `<leader>jc` | `:JupyvimAddCodeCell` | Insert a `# %%` code cell marker |
| `<leader>jm` | `:JupyvimAddMarkdownCell` | Insert a `# %% [markdown]` cell marker |
| `<leader>jx` / `Shift+Enter` | `:JupyvimRunCell` | Run the cell under the cursor, wait for it to finish, show `⏳ Running…` → `✔ Done` as virtual text |
| `<leader>jp` | `:JupyvimPreview` | Open/focus the browser preview |
| `<leader>js` | `:JupyvimStatus` | Show backend status |
| `<leader>jl` | `:JupyvimLog` | Open the debug log |
| — | `:JupyvimRestart` | Restart the backend (e.g. after a kernel crash) |

## On the Astral stack (`uv`, `ruff`, `ty`)

You don't need to do anything special — that's the point of the mirror-file architecture. The mirror is a real `.py` file on disk, so:

- **`uv`**: kernel discovery walks up from the notebook's directory looking for `.venv/bin/python`. That's exactly the layout `uv venv` / `uv init` produce, so a `uv`-managed environment is picked up automatically. `uv add ipykernel` before first run.
- **`ruff` / `ty`** (or Pyright, or whatever): these are Neovim LSP/lint clients configured the normal way in your Neovim config, pointed at your `.venv`. Jupyvim doesn't wrap or proxy them — the mirror file is indistinguishable from any other `.py` file on disk, so they attach and run exactly as fast as they would on a plain script. All three of `uv`/`ruff`/`ty` being Rust means the whole edit loop (Rust LSP tooling + Rust notebook backend) has no Python-speed bottleneck anywhere except the kernel execution itself, which is bound by whatever code you're actually running.

One real caveat: static analysis tools don't understand notebook execution order — a `# %%` cell that only makes sense after a *different* cell ran first (out-of-order execution, a classic notebook footgun) will confuse `ruff`/`ty`/Pyright exactly as much as it would confuse you. That's inherent to the notebook-as-script model, not something Jupyvim can fix.

## HTTP API (for reference)

| Endpoint | Method | Purpose |
|---|---|---|
| `/api/notebook` | GET | Current cells, pre-rendered (syntax-highlighted code, rendered markdown), outputs |
| `/api/mirror` | GET | Path of the Jupytext mirror file |
| `/api/sync` | POST `{content}` | Neovim → backend: re-parse mirror content, persist `.ipynb`, notify browser |
| `/api/execute_cell` | POST `{index}` | Fire-and-forget execution (used by browser ▶ buttons) |
| `/api/execute_cell_wait` | POST `{index}` | Blocks until the kernel goes idle again (used by `<leader>jx` for deterministic status feedback) |
| `/ws` | WebSocket | Live iopub stream + `notebook_update` structure-change notifications |

## Known limitations

- One backend process (and one kernel) per notebook, per Neovim instance — opening a second `.ipynb`/bootstrapped `.py` while one is already active is currently a no-op guard, not a second session.
- No cell reordering/move/delete from the browser (by design — Neovim is the single source of truth for structure). Reorder cells by cutting/pasting `# %%` blocks in Neovim.
- Jupytext YAML front-matter headers aren't parsed specially (see above).
