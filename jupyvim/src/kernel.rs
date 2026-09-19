use serde::Deserialize;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use tokio::sync::{mpsc, broadcast};
use hmac::{Hmac, Mac};
use zeromq::{prelude::*, ZmqMessage};
use sha2::Sha256;
use bytes::Bytes;
use std::sync::Arc;

type HmacSha256 = Hmac<Sha256>;

#[derive(Deserialize, Debug, Clone)]
pub struct KernelConnectionInfo {
    pub transport: String,
    pub ip: String,
    pub shell_port: u16,
    pub iopub_port: u16,
    pub stdin_port: u16,
    pub control_port: u16,
    pub hb_port: u16,
    pub key: String,
    pub signature_scheme: String,
}

pub struct KernelManager {
    _process: Child,
    pub connection_info: KernelConnectionInfo,
}

impl KernelManager {
    pub fn start(project_dir: &Path, custom_python: Option<&Path>) -> Result<Self, Box<dyn std::error::Error>> {
        let python_path = match custom_python {
            Some(path) => path.to_path_buf(),
            None => Self::find_python_interpreter(project_dir),
        };

        let connection_file = project_dir.join("kernel_connection.json");
        if connection_file.exists() {
            let _ = std::fs::remove_file(&connection_file);
        }

        let connection_file_str = connection_file.to_str().unwrap();

        let child = Command::new(&python_path)
            .args(["-m", "ipykernel_launcher", "-f", connection_file_str])
            .current_dir(project_dir)
            .spawn()
            .map_err(|e| format!("Fehler beim Starten von Python unter {:?}: {}", python_path, e))?;        

        let connection_info = Self::wait_for_connection_file(&connection_file)?;

        println!("==> [Kernel] Verbunden mit Ports - Shell: {}, IOPub: {}", 
            connection_info.shell_port, connection_info.iopub_port);

        Ok(Self {
            _process: child,
            connection_info,
        })
    }

    /// Baut die Verbindung zum ZMQ-Shell-Socket auf
    pub async fn connect_shell(&self) -> Result<zeromq::DealerSocket, Box<dyn std::error::Error>> {
        let endpoint = format!(
            "{}://{}:{}",
            self.connection_info.transport,
            self.connection_info.ip,
            self.connection_info.shell_port
        );
        
        let mut socket = zeromq::DealerSocket::new();
        socket.connect(&endpoint).await?;
        println!("==> [Kernel] Verbunden mit ZMQ Shell-Socket unter {}", endpoint);
        
        Ok(socket)
    }

    fn wait_for_connection_file(path: &Path) -> Result<KernelConnectionInfo, Box<dyn std::error::Error>> {
        for _ in 0..50 {
            if path.exists() {
                if let Ok(file) = File::open(path) {
                    if let Ok(info) = serde_json::from_reader(file) {
                        return Ok(info);
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50)); 
        }
        Err("Timeout beim Warten auf die kernel_connection.json".into())
    }

    fn find_python_interpreter(project_dir: &Path) -> PathBuf {
        let venv_python = project_dir.join(".venv/bin/python");
        if venv_python.exists() {
            return venv_python;
        }
        let sub_venv_python = project_dir.join("test/.venv/bin/python");
        if sub_venv_python.exists() {
            return sub_venv_python;
        }
        PathBuf::from("python3")
    }
}

// Befehle, die an den Kernel-Actor gesendet werden können
pub enum KernelCommand {
    ExecuteCode { code: String },
}

impl KernelManager {
    /// Startet die ZMQ-Event-Loop als eigenständigen Tokio-Task (Actor-Modell) mit HMAC-Signierung
    pub async fn run_actor(
        mut socket: zeromq::DealerSocket,
        mut rx: mpsc::Receiver<KernelCommand>,
        secret_key: String,
    ) {
        let master_mac = HmacSha256::new_from_slice(secret_key.as_bytes())
            .expect("HMAC kann mit diesem Key nicht initialisiert werden");

        while let Some(cmd) = rx.recv().await {
            match cmd {
                KernelCommand::ExecuteCode { code } => {
                    println!("==> [Kernel-Actor] Signiere und Sende Code: {}", code);

                    let mut mac = master_mac.clone();

                    let header = format!(
                        r#"{{"msg_id":"{}","username":"jupyvim","session":"jupyvim-session","msg_type":"execute_request","version":"5.3"}}"#,
                        uuid::Uuid::new_v4()
                    );
                    let parent_header = "{}";
                    let metadata = "{}";
                    let content = serde_json::json!({
                        "code": code,
                        "silent": false,
                        "store_history": true,
                        "user_expressions": {},
                        "allow_stdin": false
                    }).to_string();

                    let h_bytes = header.as_bytes();
                    let ph_bytes = parent_header.as_bytes();
                    let m_bytes = metadata.as_bytes();
                    let c_bytes = content.as_bytes();

                    mac.update(h_bytes);
                    mac.update(ph_bytes);
                    mac.update(m_bytes);
                    mac.update(c_bytes);
                    
                    let result = mac.finalize();
                    let signature = hex::encode(result.into_bytes());

                    let mut zmsg = ZmqMessage::from(Bytes::from_static(b"<IDS|MSG>"));
                    zmsg.push_back(Bytes::from(signature));
                    zmsg.push_back(Bytes::from(header));
                    zmsg.push_back(Bytes::from(parent_header));
                    zmsg.push_back(Bytes::from(metadata));
                    zmsg.push_back(Bytes::from(content));

                    if let Err(e) = socket.send(zmsg).await {
                        eprintln!("==> [Fehler] Konnte Nachricht nicht an ZMQ-Socket senden: {}", e);
                    } else {
                        println!("==> [Kernel-Actor] Execute-Request erfolgreich mit HMAC-Signatur an Shell-Socket gesendet!");
                    }
                }
            }
        }
    }
}

impl KernelManager {
    /// Baut die Verbindung zum ZMQ-IOPub-Socket auf und abonniert alle Topics
    pub async fn connect_iopub(&self) -> Result<zeromq::SubSocket, Box<dyn std::error::Error>> {
        let endpoint = format!(
            "{}://{}:{}",
            self.connection_info.transport,
            self.connection_info.ip,
            self.connection_info.iopub_port
        );
        
        let mut socket = zeromq::SubSocket::new();
        socket.connect(&endpoint).await?;
        socket.subscribe("").await?;
        
        println!("==> [Kernel] Verbunden mit ZMQ IOPub-Socket unter {}", endpoint);
        Ok(socket)
    }

    /// Hocheffiziente Event-Loop für den IOPub-Datenstrom mit Zero-Copy Broadcast
   pub async fn run_iopub_actor(
    mut socket: zeromq::SubSocket,
    iopub_tx: broadcast::Sender<Arc<Vec<Bytes>>>,
) {
    let _ = socket.subscribe("").await;
    println!("==> [IOPub-Worker] Verbunden und bereit für den Live-Datenstrom.");

    // Ultraschneller I/O-Leseloop
    loop {
        match socket.recv().await {
            Ok(zmq_msg) => {
                // Konvertiert die ZmqMessage direkt in Vec<Bytes> (alle Frames der Multipart-Nachricht)
                let frames: Vec<Bytes> = zmq_msg.into_vec();
                
                println!("==> [IOPub-Worker] Rohnachricht empfangen mit {} Frame(s)", frames.len());

                let shared_msg = Arc::new(frames);
                let _ = iopub_tx.send(shared_msg);
            }
            Err(e) => {
                eprintln!("==> [Fehler] Konnte Nachricht nicht von IOPub-Socket empfangen: {}", e);
                break;
            }
        }
    }
} 

}
