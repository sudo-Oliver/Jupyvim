use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{sink::SinkExt, stream::StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, RwLock};
use bytes::Bytes;

use crate::kernel::KernelCommand;
use crate::notebook::{Cell, Notebook};
use crate::render;

// Embedded static assets (zero file I/O at runtime)
const HTML_CONTENT: &str = include_str!("../static/index.html");
const CSS_CONTENT: &str = include_str!("../static/style.css");
const JS_CONTENT: &str = include_str!("../static/app.js");

#[derive(Deserialize)]
pub struct SyncPayload {
    pub content: String,
    /// true (the default, used on `:w`): re-parse and also write the real
    /// .ipynb to disk. false: used for live-typing preview updates (Neovim
    /// debounces TextChanged/TextChangedI into these) -- update the
    /// in-memory notebook and push it to the browser, but never touch disk,
    /// so keystrokes never turn into filesystem writes.
    #[serde(default = "default_true")]
    pub persist: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
pub struct ExecuteCellPayload {
    pub index: usize,
    /// Optional client-supplied msg_id, so a caller (e.g. the browser's
    /// Run All loop) can register interest in this execution's iopub
    /// messages *before* sending the request, instead of racing to learn
    /// the id only after the kernel has already finished.
    #[serde(default)]
    pub msg_id: Option<String>,
}

#[derive(Serialize)]
pub struct ExecuteResponse {
    pub status: String,
    pub msg_id: String,
}

// Query parameters for WebSocket (optional filtering by parent_msg_id)
#[derive(Deserialize)]
pub struct WsParams {
    pub parent_msg_id: Option<String>,
}

// Shared server application state
pub struct AppState {
    pub ipynb_path: PathBuf,
    pub mirror_path: PathBuf,
    pub notebook: Notebook,
    pub kernel_tx: mpsc::Sender<KernelCommand>,
    pub iopub_tx: broadcast::Sender<Arc<Vec<Bytes>>>,
    /// Broadcasts pre-serialized JSON strings straight to connected browser
    /// WebSocket clients (structure changes, cursor-follow pings, ...) --
    /// kept as already-serialized text so the WS forwarding loop can just
    /// pass it through with zero extra allocation/re-encoding.
    pub ui_tx: broadcast::Sender<String>,
    /// Maps an in-flight execute_request msg_id to the cell index it targets,
    /// so the output-capture task can attribute iopub messages to a cell.
    pub exec_map: HashMap<String, usize>,
    /// `v:servername` of the Neovim instance that started this backend, if
    /// any (passed via --nvim-server). Lets the server drive the editor's
    /// cursor via `nvim --server <addr> --remote-expr` for click-to-jump.
    pub nvim_server: Option<String>,
}

pub struct WebServer {
    port: u16,
    ipynb_path: PathBuf,
    mirror_path: PathBuf,
    notebook: Notebook,
    kernel_tx: mpsc::Sender<KernelCommand>,
    iopub_tx: broadcast::Sender<Arc<Vec<Bytes>>>,
    ui_tx: broadcast::Sender<String>,
    nvim_server: Option<String>,
}

impl WebServer {
    pub fn new(
        port: u16,
        ipynb_path: PathBuf,
        mirror_path: PathBuf,
        notebook: Notebook,
        kernel_tx: mpsc::Sender<KernelCommand>,
        iopub_tx: broadcast::Sender<Arc<Vec<Bytes>>>,
        ui_tx: broadcast::Sender<String>,
        nvim_server: Option<String>,
    ) -> Self {
        Self {
            port,
            ipynb_path,
            mirror_path,
            notebook,
            kernel_tx,
            iopub_tx,
            ui_tx,
            nvim_server,
        }
    }

    pub async fn run(self) -> Result<(), Box<dyn std::error::Error>> {
        let state = Arc::new(RwLock::new(AppState {
            ipynb_path: self.ipynb_path,
            mirror_path: self.mirror_path,
            notebook: self.notebook,
            kernel_tx: self.kernel_tx,
            iopub_tx: self.iopub_tx,
            ui_tx: self.ui_tx,
            exec_map: HashMap::new(),
            nvim_server: self.nvim_server,
        }));

        tokio::spawn(capture_outputs_task(state.clone()));

        let app = Router::new()
            .route("/", get(handle_index))
            .route("/style.css", get(handle_css))
            .route("/app.js", get(handle_js))
            .route("/api/notebook", get(handle_notebook))
            .route("/api/mirror", get(handle_mirror))
            .route("/api/sync", post(handle_sync))
            .route("/api/execute_cell", post(handle_execute_cell))
            .route("/api/execute_cell_wait", post(handle_execute_cell_wait))
            .route("/api/cursor_moved", post(handle_cursor_moved))
            .route("/ws", get(handle_websocket))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", self.port)).await?;
        println!("==> [Server] Starting Axum server on http://127.0.0.1:{}", self.port);

        axum::serve(listener, app).await?;
        Ok(())
    }
}

// Serves embedded index.html
async fn handle_index() -> impl IntoResponse {
    Html(HTML_CONTENT)
}

// Serves embedded CSS
async fn handle_css() -> impl IntoResponse {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "text/css; charset=utf-8".parse().unwrap());
    (headers, CSS_CONTENT)
}

// Serves embedded JavaScript
async fn handle_js() -> impl IntoResponse {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/javascript; charset=utf-8".parse().unwrap());
    (headers, JS_CONTENT)
}

// Returns the current in-memory notebook state, pre-rendered (syntax-highlighted
// code, rendered markdown) so the browser stays a lightweight read-only viewer.
async fn handle_notebook(State(state): State<Arc<RwLock<AppState>>>) -> Response {
    // Write lock, not read: cells whose source hasn't changed since the last
    // fetch already carry a cached `rendered_html` (see Notebook::from_percent),
    // so this only ever pays the syntect/pulldown-cmark cost for cells that
    // are actually new or edited, not the whole notebook on every debounced
    // live-typing sync.
    let mut state_guard = state.write().await;
    let file_name = state_guard
        .ipynb_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Untitled.ipynb")
        .to_string();

    let line_starts = state_guard.notebook.line_starts();

    let cells: Vec<serde_json::Value> = state_guard
        .notebook
        .cells
        .iter_mut()
        .enumerate()
        .map(|(i, cell)| {
            let line_start = line_starts.get(i).copied().unwrap_or(1);
            match cell.cell_type.as_str() {
                "markdown" => {
                    let html = cell
                        .rendered_html
                        .get_or_insert_with(|| render::render_markdown(&cell.source));
                    serde_json::json!({
                        "cell_type": "markdown",
                        "source": cell.source,
                        "html": html,
                        "line_start": line_start,
                    })
                }
                "raw" => serde_json::json!({
                    "cell_type": "raw",
                    "source": cell.source,
                    "line_start": line_start,
                }),
                _ => {
                    let html = cell
                        .rendered_html
                        .get_or_insert_with(|| render::highlight_python(&cell.source));
                    serde_json::json!({
                        "cell_type": "code",
                        "source": cell.source,
                        "source_html": html,
                        "outputs": cell.outputs,
                        "execution_count": cell.execution_count,
                        "line_start": line_start,
                    })
                }
            }
        })
        .collect();

    let response = serde_json::json!({
        "filename": file_name,
        "path": state_guard.ipynb_path.to_string_lossy(),
        "cells": cells,
    });

    Json(response).into_response()
}

// Returns the path of the Jupytext-style plain-text mirror that Neovim edits.
async fn handle_mirror(State(state): State<Arc<RwLock<AppState>>>) -> Response {
    let state_guard = state.read().await;
    Json(serde_json::json!({
        "mirror_path": state_guard.mirror_path.to_string_lossy(),
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct CursorMovedPayload {
    pub index: usize,
}

// Neovim -> browser cursor-follow: Neovim only calls this when the cursor
// crosses into a different cell (see cell_at_cursor's dedup in the Lua
// plugin), so this is a rare event, not a per-keystroke one. Just relays
// the cell index to connected browsers over the existing WebSocket -- no
// polling, no extra connection.
async fn handle_cursor_moved(
    State(state): State<Arc<RwLock<AppState>>>,
    Json(payload): Json<CursorMovedPayload>,
) -> impl IntoResponse {
    let state_guard = state.read().await;
    let _ = state_guard
        .ui_tx
        .send(json!({"type": "cursor_at_cell", "index": payload.index}).to_string());
    StatusCode::OK
}

// Receives the up-to-date mirror content from Neovim, re-parses it into
// notebook cells (carrying over prior outputs/execution_count), and notifies
// connected browsers to refresh. Only writes the real .ipynb to disk when
// `persist` is true (a real `:w`) -- debounced live-typing updates
// (`persist: false`) stay entirely in memory, so keystrokes never touch disk.
async fn handle_sync(
    State(state): State<Arc<RwLock<AppState>>>,
    Json(payload): Json<SyncPayload>,
) -> impl IntoResponse {
    let mut state_guard = state.write().await;
    let new_notebook = Notebook::from_percent(&payload.content, Some(&state_guard.notebook));

    if !payload.persist {
        state_guard.notebook = new_notebook;
        let _ = state_guard.ui_tx.send(json!({"type": "notebook_update"}).to_string());
        return (StatusCode::OK, Json(serde_json::json!({"status": "ok"})));
    }

    match std::fs::write(
        &state_guard.ipynb_path,
        serde_json::to_string_pretty(&new_notebook.to_ipynb_value()).unwrap_or_default(),
    ) {
        Ok(_) => {
            state_guard.notebook = new_notebook;
            let _ = state_guard.ui_tx.send(json!({"type": "notebook_update"}).to_string());
            (StatusCode::OK, Json(serde_json::json!({"status": "ok"})))
        }
        Err(e) => {
            eprintln!("==> [Server Error] Failed to write notebook file: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"status": "error", "message": e.to_string()})),
            )
        }
    }
}

// Executes a cell by index and returns immediately once the request has been
// forwarded to the kernel (fire-and-forget; used by the browser's Run button).
async fn handle_execute_cell(
    State(state): State<Arc<RwLock<AppState>>>,
    Json(payload): Json<ExecuteCellPayload>,
) -> impl IntoResponse {
    let mut state_guard = state.write().await;
    let Some(cell) = state_guard.notebook.cells.get(payload.index) else {
        return (
            StatusCode::NOT_FOUND,
            Json(ExecuteResponse { status: "error".to_string(), msg_id: String::new() }),
        );
    };
    if cell.cell_type != "code" {
        return (
            StatusCode::BAD_REQUEST,
            Json(ExecuteResponse { status: "error".to_string(), msg_id: String::new() }),
        );
    }
    let code = cell.source.clone();

    let msg_id = payload.msg_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    state_guard.exec_map.insert(msg_id.clone(), payload.index);
    match state_guard
        .kernel_tx
        .send(KernelCommand::ExecuteCode { code, msg_id: msg_id.clone() })
        .await
    {
        Ok(_) => (
            StatusCode::OK,
            Json(ExecuteResponse { status: "ok".to_string(), msg_id }),
        ),
        Err(e) => {
            eprintln!("==> [Server Error] Failed to send command to kernel: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ExecuteResponse { status: "error".to_string(), msg_id }),
            )
        }
    }
}

// Executes a cell by index and blocks until the kernel reports it idle again
// (or a timeout elapses). Used by Neovim's <leader>jx so the job's on_exit
// callback can clear a "Running..." virtual-text marker deterministically.
async fn handle_execute_cell_wait(
    State(state): State<Arc<RwLock<AppState>>>,
    Json(payload): Json<ExecuteCellPayload>,
) -> impl IntoResponse {
    let msg_id = payload.msg_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let (code, kernel_tx, mut iopub_rx) = {
        let mut state_guard = state.write().await;
        let Some(cell) = state_guard.notebook.cells.get(payload.index) else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"status": "error", "msg_id": ""})),
            );
        };
        if cell.cell_type != "code" {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"status": "error", "msg_id": ""})),
            );
        }
        let code = cell.source.clone();
        state_guard.exec_map.insert(msg_id.clone(), payload.index);
        (
            code,
            state_guard.kernel_tx.clone(),
            state_guard.iopub_tx.subscribe(),
        )
    };

    if kernel_tx
        .send(KernelCommand::ExecuteCode { code, msg_id: msg_id.clone() })
        .await
        .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"status": "error", "msg_id": msg_id})),
        );
    }

    let mut error_info: Option<Value> = None;
    let wait_result = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            match iopub_rx.recv().await {
                Ok(frames) => {
                    if let Some((msg_type, parent_id, content)) = parse_iopub_frame(&frames) {
                        if parent_id.as_deref() != Some(msg_id.as_str()) {
                            continue;
                        }
                        if msg_type == "error" {
                            error_info = Some(json!({
                                "ename": content.get("ename").cloned().unwrap_or(Value::Null),
                                "evalue": content.get("evalue").cloned().unwrap_or(Value::Null),
                            }));
                        } else if msg_type == "status"
                            && content.get("execution_state").and_then(|v| v.as_str()) == Some("idle")
                        {
                            return;
                        }
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            }
        }
    })
    .await;

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "msg_id": msg_id,
            "timed_out": wait_result.is_err(),
            "error": error_info,
        })),
    )
}

// Background task: attributes iopub messages to the cell that triggered them
// (via AppState.exec_map) and folds them into that cell's persisted outputs,
// so outputs survive page reloads and are written into the saved .ipynb —
// the live WebSocket stream to the browser is a separate, faster path.
async fn capture_outputs_task(state: Arc<RwLock<AppState>>) {
    let mut rx = {
        let guard = state.read().await;
        guard.iopub_tx.subscribe()
    };

    loop {
        match rx.recv().await {
            Ok(frames) => {
                let Some((msg_type, Some(parent_id), content)) = parse_iopub_frame(&frames) else {
                    continue;
                };

                let mut guard = state.write().await;
                let Some(&index) = guard.exec_map.get(&parent_id) else {
                    continue;
                };

                let is_idle = msg_type == "status"
                    && content.get("execution_state").and_then(|v| v.as_str()) == Some("idle");

                if let Some(cell) = guard.notebook.cells.get_mut(index) {
                    apply_iopub_to_cell(cell, &msg_type, &content);
                }

                if is_idle {
                    guard.exec_map.remove(&parent_id);
                }
            }
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => break,
        }
    }
}

fn apply_iopub_to_cell(cell: &mut Cell, msg_type: &str, content: &Value) {
    match msg_type {
        "execute_input" => {
            cell.outputs.clear();
            cell.execution_count = content.get("execution_count").and_then(|v| v.as_i64());
        }
        "stream" => {
            let name = content.get("name").and_then(|v| v.as_str()).unwrap_or("stdout");
            let text = content.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(last) = cell.outputs.last_mut() {
                if last.get("output_type").and_then(|v| v.as_str()) == Some("stream")
                    && last.get("name").and_then(|v| v.as_str()) == Some(name)
                {
                    let existing = last.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    let merged = format!("{}{}", existing, text);
                    last["text"] = json!(merged);
                    return;
                }
            }
            cell.outputs.push(json!({
                "output_type": "stream",
                "name": name,
                "text": text,
            }));
        }
        "execute_result" | "display_data" => {
            let mut obj = json!({
                "output_type": msg_type,
                "data": content.get("data").cloned().unwrap_or_else(|| json!({})),
                "metadata": content.get("metadata").cloned().unwrap_or_else(|| json!({})),
            });
            if msg_type == "execute_result" {
                obj["execution_count"] = content
                    .get("execution_count")
                    .cloned()
                    .unwrap_or(Value::Null);
            }
            cell.outputs.push(obj);
        }
        "error" => {
            cell.outputs.push(json!({
                "output_type": "error",
                "ename": content.get("ename").cloned().unwrap_or(Value::Null),
                "evalue": content.get("evalue").cloned().unwrap_or(Value::Null),
                "traceback": content.get("traceback").cloned().unwrap_or_else(|| json!([])),
            }));
        }
        _ => {}
    }
}

// Click-to-jump: rather than adding a msgpack-RPC client dependency to talk
// to Neovim's RPC socket directly, this shells out to the `nvim` binary
// itself (already guaranteed to be on PATH -- it's what's running the
// plugin) using its built-in `--server`/`--remote-expr` remote-control
// flags. One `jupyvim.jump_to_line()` Lua call finds the right window,
// moves the cursor, and centers it -- see lua/jupyvim/init.lua. Fire-and-
// forget: a stray click when no Neovim server is registered, or Neovim
// exiting mid-flight, just silently does nothing, never blocks the server.
fn jump_neovim_to_line(nvim_server: Option<&str>, line: u64) {
    let Some(addr) = nvim_server else { return };
    let expr = format!("v:lua.require('jupyvim').jump_to_line({})", line);
    // tokio::process::Command (not std::process): its Child is reaped by
    // tokio's orphan reaper even if dropped without an explicit .wait(),
    // so a burst of clicks can't accumulate zombie processes over an
    // hours-long session.
    match tokio::process::Command::new("nvim")
        .args(["--server", addr, "--remote-expr", &expr])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => {}
        Err(e) => eprintln!("==> [Server] Failed to spawn nvim --server for click-to-jump: {}", e),
    }
}

fn parse_iopub_frame(frames: &[Bytes]) -> Option<(String, Option<String>, serde_json::Value)> {
    if frames.len() < 7 {
        return None;
    }
    let header_val: serde_json::Value = serde_json::from_slice(&frames[3]).ok()?;
    let parent_val: Option<serde_json::Value> = serde_json::from_slice(&frames[4]).ok();
    let content_val: serde_json::Value = serde_json::from_slice(&frames[6]).ok()?;

    let msg_type = header_val.get("msg_type")?.as_str()?.to_string();
    let parent_msg_id = parent_val
        .as_ref()
        .and_then(|p| p.get("msg_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Some((msg_type, parent_msg_id, content_val))
}

// WebSocket endpoint with optional query parameters
async fn handle_websocket(
    ws: WebSocketUpgrade,
    State(state): State<Arc<RwLock<AppState>>>,
    Query(params): Query<WsParams>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket_connection(socket, state, params.parent_msg_id))
}

async fn handle_socket_connection(
    socket: WebSocket,
    state: Arc<RwLock<AppState>>,
    target_parent_id: Option<String>,
) {
    let (mut sender, mut receiver) = socket.split();

    let state_guard = state.read().await;
    let iopub_tx = state_guard.iopub_tx.clone();
    let ui_tx = state_guard.ui_tx.clone();
    let nvim_server = state_guard.nvim_server.clone();
    drop(state_guard);

    // 1. Task for incoming WebSocket messages. The browser is a read-only
    // viewer that executes cells via the HTTP endpoints, so the only inbound
    // message it sends is a click-to-jump request.
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            match msg {
                Message::Text(text) => {
                    if let Ok(payload) = serde_json::from_str::<Value>(&text) {
                        if payload.get("event").and_then(|v| v.as_str()) == Some("jump_to_line") {
                            if let Some(line) = payload.get("line").and_then(|v| v.as_u64()) {
                                jump_neovim_to_line(nvim_server.as_deref(), line);
                            }
                        }
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // 2. Task for outgoing IOPub data + UI structure updates (Server -> Browser)
    let mut send_task = tokio::spawn(async move {
        let mut iopub_rx = iopub_tx.subscribe();
        let mut ui_rx = ui_tx.subscribe();
        println!("==> [WebSocket] New client connected. Filter Parent ID: {:?}", target_parent_id);

        loop {
            tokio::select! {
                iopub_result = iopub_rx.recv() => {
                    match iopub_result {
                        Ok(shared_frames) => {
                            if let Some((msg_type, parent_msg_id, content_val)) = parse_iopub_frame(&shared_frames) {
                                if let Some(ref target_id) = target_parent_id {
                                    if parent_msg_id.as_deref() != Some(target_id.as_str()) {
                                        continue;
                                    }
                                }

                                let ws_message = serde_json::json!({
                                    "type": "iopub",
                                    "msg_type": msg_type,
                                    "parent_msg_id": parent_msg_id,
                                    "content": content_val
                                });

                                if let Ok(json_str) = serde_json::to_string(&ws_message) {
                                    if sender.send(Message::Text(json_str)).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                        Err(RecvError::Lagged(skipped)) => {
                            eprintln!("==> [WebSocket] Warning: Client dropped {} messages!", skipped);
                            let warning_json = serde_json::json!({
                                "type": "error",
                                "message": format!("Output stream lagged, {} messages dropped", skipped)
                            }).to_string();

                            if sender.send(Message::Text(warning_json)).await.is_err() {
                                break;
                            }
                        }
                        Err(RecvError::Closed) => break,
                    }
                }
                ui_result = ui_rx.recv() => {
                    match ui_result {
                        // Already-serialized JSON (see AppState.ui_tx doc comment) --
                        // forwarded as-is, no re-wrapping needed.
                        Ok(json_str) => {
                            if sender.send(Message::Text(json_str)).await.is_err() {
                                break;
                            }
                        }
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => break,
                    }
                }
            }
        }
    });

    // Cleanup: if either task finishes, cancel the other
    tokio::select! {
        _ = (&mut recv_task) => send_task.abort(),
        _ = (&mut send_task) => recv_task.abort(),
    }

    println!("==> [Server] WebSocket connection closed and resources cleaned up.");
}
