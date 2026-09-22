#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod kernel;
mod notebook;
mod render;
mod server;

use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use bytes::Bytes;
use kernel::{KernelManager, KernelCommand};
use notebook::Notebook;
use server::WebServer;
use tokio::sync::mpsc;

// CLI arguments passed at startup (e.g. path to notebook)
#[derive(Parser, Debug)]
#[command(name = "jupyvim", version, about = "High-performance Jupyter backend for Neovim")]
struct Args {
    /// Path to notebook or markdown file
    #[arg(short, long)]
    file: PathBuf,

    /// Port for local web server
    #[arg(short, long, default_value_t = 3000)]
    port: u16,

    /// Optional path to custom Python interpreter
    #[arg(short = 'y', long)]
    python: Option<PathBuf>,

    /// v:servername of the Neovim instance that started this backend, if
    /// any. Enables click-to-jump (browser -> Neovim cursor).
    #[arg(long)]
    nvim_server: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let file_str = args.file.to_string_lossy().to_string();
    println!("==> [jupyvim] Starting for file: {}", file_str);

    let project_dir = if let Some(parent) = args.file.parent() {
        if parent.as_os_str().is_empty() {
            std::env::current_dir()?
        } else {
            std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf())
        }
    } else {
        std::env::current_dir()?
    };
    println!("==> [jupyvim] Project directory: {:?}", project_dir);

    let given_path = if args.file.is_absolute() {
        args.file.clone()
    } else {
        project_dir.join(args.file.file_name().unwrap_or_default())
    };

    let is_py_entry = given_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("py"))
        .unwrap_or(false);

    let stem = given_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("notebook")
        .to_string();

    // Two entry modes:
    //  - `.ipynb` given: that file is the ground truth; a hidden Jupytext
    //    "percent" mirror is (re)generated in `.jupyvim/` for Neovim to edit.
    //  - `.py` given: the user is bootstrapping a fresh notebook straight
    //    from a plain script (e.g. a `uv init` scaffold). That file becomes
    //    the mirror directly (no hidden copy needed) and a sibling `.ipynb`
    //    is created/derived as the persisted ground truth.
    let (ipynb_path, mirror_path, notebook) = if is_py_entry {
        let ipynb_path = given_path.with_extension("ipynb");
        let notebook = match std::fs::read_to_string(&ipynb_path) {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(value) => Notebook::from_ipynb_value(&value),
                Err(_) => Notebook::empty(),
            },
            Err(_) => {
                let script = std::fs::read_to_string(&given_path).unwrap_or_default();
                if script.trim().is_empty() {
                    Notebook::empty()
                } else {
                    Notebook::from_percent(&script, None)
                }
            }
        };
        std::fs::write(&ipynb_path, serde_json::to_string_pretty(&notebook.to_ipynb_value())?)?;
        (ipynb_path, given_path, notebook)
    } else {
        let ipynb_path = given_path;
        let notebook = match std::fs::read_to_string(&ipynb_path) {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(value) => Notebook::from_ipynb_value(&value),
                Err(_) => Notebook::empty(),
            },
            Err(_) => Notebook::empty(),
        };

        let mirror_dir = project_dir.join(".jupyvim");
        std::fs::create_dir_all(&mirror_dir)?;
        let mirror_path = mirror_dir.join(format!("{}.py", stem));
        (ipynb_path, mirror_path, notebook)
    };

    std::fs::write(&mirror_path, notebook.to_percent())?;
    println!("==> [jupyvim] Notebook (ground truth): {:?}", ipynb_path);
    println!("==> [jupyvim] Mirror written to: {:?}", mirror_path);

    // 1. Start Jupyter kernel
    let kernel_manager = KernelManager::start(&project_dir, args.python.as_deref())?;

    // 2. Connect Shell and IOPub sockets
    let shell_socket = kernel_manager.connect_shell().await?;
    let iopub_socket = kernel_manager.connect_iopub().await?;

    // 3. Create command channel (MPSC)
    let (tx, rx) = mpsc::channel::<KernelCommand>(100);
    let secret_key = kernel_manager.connection_info.key.clone();

    // 4. Setup broadcast channels for IOPub live stream and UI structure updates
    let (iopub_tx, _) = tokio::sync::broadcast::channel::<Arc<Vec<Bytes>>>(2048);
    let (ui_tx, _) = tokio::sync::broadcast::channel::<String>(64);

    // 5. Spawn Shell actor and IOPub actor in background
    tokio::spawn(KernelManager::run_actor(shell_socket, rx, secret_key));
    tokio::spawn(KernelManager::run_iopub_actor(iopub_socket, iopub_tx.clone()));

    // 6. Initialize and run web server
    let server = WebServer::new(
        args.port,
        ipynb_path,
        mirror_path,
        notebook,
        tx,
        iopub_tx,
        ui_tx,
        args.nvim_server,
    );

    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    tokio::select! {
        res = server.run() => {
            if let Err(e) = res {
                eprintln!("==> [Error] Web server error: {}", e);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            println!("\n==> [jupyvim] Ctrl+C received. Shutting down daemon and kernel...");
        }
        _ = async {
            #[cfg(unix)]
            {
                sigterm.recv().await;
            }
            #[cfg(not(unix))]
            {
                std::future::pending::<()>().await;
            }
        } => {
            println!("\n==> [jupyvim] SIGTERM received. Shutting down daemon and kernel...");
        }
    }

    // Dropping kernel_manager cleanly terminates the child ipykernel process
    drop(kernel_manager);
    println!("==> [jupyvim] Daemon stopped successfully.");

    Ok(())
}
