use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, State},
    response::IntoResponse,
    routing::get,
    Router,
};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use crate::kernel::KernelCommand;

pub struct WebServer {
    port: u16,
    tx: mpsc::Sender<KernelCommand>,
}

impl WebServer {
    pub fn new(port: u16, tx: mpsc::Sender<KernelCommand>) -> Self {
        Self { port, tx }
    }

    /// Startet den Axum HTTP- und WebSocket-Server asynchronously
    pub async fn run(self) -> Result<(), Box<dyn std::error::Error>> {
        let app = Router::new()
            .route("/", get(index_handler))
            .route("/ws", get(websocket_handler))
            .with_state(self.tx); // Übergibt den Sender als Shared State an die Routen

        let addr = SocketAddr::from(([127, 0, 0, 1], self.port));
        println!("==> [Server] Starte Axum-Server auf http://{}", addr);

        let listener = TcpListener::bind(&addr).await?;
        axum::serve(listener, app).await?;

        Ok(())
    }
}

/// Einfacher HTTP-Handler für den Start (liefert später das Frontend aus)
async fn index_handler() -> impl IntoResponse {
    "<html><body><h2>jupyvim Live-View aktiv</h2><p>Verbindung via WebSocket wird aufgebaut...</p></body></html>"
}

/// WebSocket-Handler für die High-Performance Brücke zum ZeroMQ-Kernel
async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(tx): State<mpsc::Sender<KernelCommand>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, tx))
}

async fn handle_socket(mut socket: WebSocket, tx: mpsc::Sender<KernelCommand>) {
    println!("==> [WebSocket] Browser erfolgreich verbunden.");

    while let Some(result) = socket.recv().await {
        if let Ok(msg) = result {
            match msg {
                Message::Text(text) => {
                    println!("==> [ZMQ-Bridge] Empfangen vom Browser: {}", text);
                    
                    // Leitet den Befehl asynchron über den MPSC-Kanal an den Kernel-Actor weiter
                    let _ = tx.send(KernelCommand::ExecuteCode { code: text.to_string() }).await;

                    // Echo zurück an den Browser zum Testen
                    if socket.send(Message::Text(format!("Echo: {}", text))).await.is_err() {
                        break;
                    }   
                }
                Message::Close(_) => {
                    println!("==> [WebSocket] Verbindung vom Browser geschlossen.");
                    break;
                }
                _ => {}
            }
        } else {
            break;
        }
    }
}
