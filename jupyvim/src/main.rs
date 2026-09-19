mod kernel;
mod server;

use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc; // <--- Wichtig für den Arc-Typ im Broadcast-Kanal
use bytes::Bytes;    // <--- Wichtig für Bytes
use kernel::{KernelManager, KernelCommand};
use server::WebServer;
use tokio::sync::mpsc;

// CLI-Argumente, die beim Start übergeben werden (z.B. der Pfad zum Notebook)
#[derive(Parser, Debug)]
#[command(name = "jupyvim", version, about = "High-performance Jupyter backend for Neovim")]
struct Args {
    /// Pfad zur zu öffnenden Notebook- oder Markdown-Datei
    #[arg(short, long)]
    file: PathBuf,

    /// Port für den lokalen Webserver
    #[arg(short, long, default_value_t = 3000)]
    port: u16,

    /// Optionaler, manueller Pfad zum Python-Interpreter (z.B. für custom Envs)
    #[arg(short = 'y', long)]
    python: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    println!("==> [jupyvim] Starte für Datei: {:?}", args.file);

    let current_dir = std::env::current_dir()?;

    // 1. Kernel starten
    let kernel_manager = KernelManager::start(&current_dir, args.python.as_deref())?;

    // 2. Shell- und IOPub-Sockets verbinden
    let shell_socket = kernel_manager.connect_shell().await?;
    let iopub_socket = kernel_manager.connect_iopub().await?;

    // 3. Command-Channel (MPSC) erstellen
    let (tx, rx) = mpsc::channel::<KernelCommand>(100);
    let secret_key = kernel_manager.connection_info.key.clone();

    // --- NEU: 4. Broadcast-Kanal für den IOPub-Live-Stream einrichten ---
    // Kapazität von 2048 Nachrichten als Puffer gegen Slow Receiver
    let (iopub_tx, _) = tokio::sync::broadcast::channel::<Arc<Vec<Bytes>>>(2048);

    // 5. Shell-Actor & IOPub-Actor im Hintergrund starten
    tokio::spawn(KernelManager::run_actor(shell_socket, rx, secret_key));
    
    // IOPub-Worker bekommt jetzt den iopub_tx Sender übergeben!
    tokio::spawn(KernelManager::run_iopub_actor(iopub_socket, iopub_tx.clone()));

    // 6. Webserver initialisieren und iopub_tx mit übergeben
    let server = WebServer::new(args.port, tx, iopub_tx);

    tokio::select! {
        res = server.run() => {
            if let Err(e) = res {
                eprintln!("==> [Fehler] Webserver-Fehler: {}", e);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            println!("\n==> [jupyvim] Strg+C empfangen. Beende Daemon und fahre Kernel herunter...");
        }
    }

    Ok(())
}
