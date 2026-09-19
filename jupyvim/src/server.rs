use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use futures::{sink::SinkExt, stream::StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock, broadcast};
use tokio::sync::broadcast::error::RecvError;
use bytes::Bytes;

use crate::kernel::KernelCommand;

// Struktur für einkommende Code-/Zell-Ausführungsanfragen
#[derive(Deserialize, Serialize, Debug)]
pub struct ExecutePayload {
    pub code: String,
    pub cell_id: Option<String>,
}

// Query-Parameter für den WebSocket (Säule 3: Filterung nach parent_msg_id)
#[derive(Deserialize)]
pub struct WsParams {
    pub parent_msg_id: Option<String>,
}

// Geteilter Zustand des Webservers
pub struct AppState {
    pub kernel_tx: mpsc::Sender<KernelCommand>,
    pub iopub_tx: broadcast::Sender<Arc<Vec<Bytes>>>,
}

pub struct WebServer {
    port: u16,
    kernel_tx: mpsc::Sender<KernelCommand>,
    iopub_tx: broadcast::Sender<Arc<Vec<Bytes>>>,
}

impl WebServer {
    pub fn new(
        port: u16, 
        kernel_tx: mpsc::Sender<KernelCommand>, 
        iopub_tx: broadcast::Sender<Arc<Vec<Bytes>>>
    ) -> Self {
        Self { port, kernel_tx, iopub_tx }
    }

    pub async fn run(self) -> Result<(), Box<dyn std::error::Error>> {
        let state = Arc::new(RwLock::new(AppState {
            kernel_tx: self.kernel_tx,
            iopub_tx: self.iopub_tx,
        }));

        let app = Router::new()
            .route("/api/execute", post(handle_http_execute))
            .route("/ws", get(handle_websocket)) // Unterstützt jetzt z.B. /ws?parent_msg_id=xyz
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", self.port)).await?;
        println!("==> [Server] Starte Axum-Server auf http://127.0.0.1:{}", self.port);

        axum::serve(listener, app).await?;
        Ok(())
    }
}

// HTTP-Endpunkt für direkte Ausführungs-Requests
async fn handle_http_execute(
    State(state): State<Arc<RwLock<AppState>>>,
    Json(payload): Json<ExecutePayload>,
) -> impl IntoResponse {
    let state_guard = state.read().await;
    
    match state_guard.kernel_tx.send(KernelCommand::ExecuteCode { code: payload.code }).await {
        Ok(_) => {
            println!("==> [Server] HTTP-Execute-Request erfolgreich an Kernel weitergeleitet.");
            StatusCode::OK.into_response()
        }
        Err(e) => {
            eprintln!("==> [Server-Fehler] Konnte Befehl nicht an Kernel senden: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// WebSocket-Endpunkt mit Abfangen von Query-Parametern
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
    let kernel_tx = state_guard.kernel_tx.clone();
    let iopub_tx = state_guard.iopub_tx.clone();
    drop(state_guard); // Lock sofort freigeben!

    // 1. Task für eingehende Nachrichten vom WebSocket (Client -> Kernel)
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            match msg {
                Message::Text(text) => {
                    if let Ok(payload) = serde_json::from_str::<ExecutePayload>(&text) {
                        let _ = kernel_tx.send(KernelCommand::ExecuteCode { code: payload.code }).await;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // 2. Task für ausgehende IOPub-Daten (Kernel -> Broadcast -> WebSocket mit allen 4 Säulen)
    let mut send_task = tokio::spawn(async move {
        let mut rx = iopub_tx.subscribe();
        println!("==> [WebSocket] Neuer Client verbunden. Filter-Parent-ID: {:?}", target_parent_id);

        loop {
            match rx.recv().await {
                Ok(shared_frames) => {
                    // Säule 3: Effizientes Filtern über den Parent Header
                    if let Some(ref target_id) = target_parent_id {
                        if shared_frames.len() > 3 {
                            let parent_header_str = String::from_utf8_lossy(&shared_frames[3]);
                            if !parent_header_str.contains(target_id) {
                                continue; // Nicht für diesen Client bestimmt -> überspringen
                            }
                        }
                    }

                    // Säule 2 & 1: Parsing geschieht erst hier im Task, Zero-Copy Arc wird genutzt
                    if shared_frames.len() > 5 {
                        let content_bytes = &shared_frames[5];
                        if let Ok(text) = std::str::from_utf8(content_bytes) {
                            if sender.send(Message::Text(text.to_string())).await.is_err() {
                                break; // Client hat Verbindung getrennt
                            }
                        }
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    // Säule 4: Lagging-Schutz bei Überlastung
                    eprintln!("==> [WebSocket] Warnung: Client hat {} Nachrichten verpasst!", skipped);
                    
                    let warning_json = format!(
                        r#"{{"type":"error","message":"Ausgabestrom überlastet, {} Nachrichten übersprungen"}}"#,
                        skipped
                    );
                    
                    if sender.send(Message::Text(warning_json)).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Closed) => {
                    break;
                }
            }
        }
    });

    // Cleanup: Wenn ein Task abbricht, wird der andere beendet
    tokio::select! {
        _ = (&mut recv_task) => send_task.abort(),
        _ = (&mut send_task) => recv_task.abort(),
    }

    println!("==> [Server] WebSocket-Verbindung geschlossen und Ressourcen bereinigt.");
}
