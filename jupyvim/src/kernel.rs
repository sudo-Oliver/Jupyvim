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

// stdin/control/hb ports and signature_scheme are part of the Jupyter
// connection-file schema but unused here -- Jupyvim only ever opens the
// Shell and IOPub sockets and always signs with HMAC-SHA256, so serde simply
// ignores those fields from the JSON rather than the struct carrying dead
// weight for them.
#[derive(Deserialize, Debug, Clone)]
pub struct KernelConnectionInfo {
    pub transport: String,
    pub ip: String,
    pub shell_port: u16,
    pub iopub_port: u16,
    pub key: String,
}

pub struct KernelManager {
    process: Child,
    pub connection_info: KernelConnectionInfo,
    pub connection_file: PathBuf,
}

impl Drop for KernelManager {
    fn drop(&mut self) {
        println!("==> [KernelManager] Shutting down kernel (PID: {})...", self.process.id());
        let _ = self.process.kill();
        let _ = self.process.wait();
        if self.connection_file.exists() {
            let _ = std::fs::remove_file(&self.connection_file);
        }
    }
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
            .map_err(|e| format!("Error launching Python at {:?}: {}", python_path, e))?;        

        let connection_info = Self::wait_for_connection_file(&connection_file)?;

        println!("==> [Kernel] Connected to ports - Shell: {}, IOPub: {}", 
            connection_info.shell_port, connection_info.iopub_port);

        Ok(Self {
            process: child,
            connection_info,
            connection_file,
        })
    }

    /// Connects to ZMQ Shell socket
    pub async fn connect_shell(&self) -> Result<zeromq::DealerSocket, Box<dyn std::error::Error>> {
        let endpoint = format!(
            "{}://{}:{}",
            self.connection_info.transport,
            self.connection_info.ip,
            self.connection_info.shell_port
        );
        
        let mut socket = zeromq::DealerSocket::new();
        socket.connect(&endpoint).await?;
        println!("==> [Kernel] Connected to ZMQ Shell socket at {}", endpoint);
        
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
        Err("Timeout waiting for kernel_connection.json".into())
    }

    fn find_python_interpreter(project_dir: &Path) -> PathBuf {
        let mut curr = Some(project_dir);
        while let Some(dir) = curr {
            let venv_python = dir.join(".venv/bin/python");
            if venv_python.exists() {
                return venv_python;
            }
            curr = dir.parent();
        }
        let sub_venv_python = project_dir.join("test/.venv/bin/python");
        if sub_venv_python.exists() {
            return sub_venv_python;
        }
        PathBuf::from("python3")
    }
}

// Commands sent to the kernel actor
pub enum KernelCommand {
    ExecuteCode { code: String, msg_id: String },
}

impl KernelManager {
    /// Starts ZMQ event loop as a background Tokio task (actor model) with HMAC signing
    pub async fn run_actor(
        mut socket: zeromq::DealerSocket,
        mut rx: mpsc::Receiver<KernelCommand>,
        secret_key: String,
    ) {
        let master_mac = HmacSha256::new_from_slice(secret_key.as_bytes())
            .expect("HMAC could not be initialized with this key");

        while let Some(cmd) = rx.recv().await {
            match cmd {
                KernelCommand::ExecuteCode { code, msg_id } => {
                    let effective_msg_id = if msg_id.is_empty() {
                        uuid::Uuid::new_v4().to_string()
                    } else {
                        msg_id
                    };

                    println!("==> [Kernel-Actor] Signing and sending code (msg_id: {}): {}", effective_msg_id, code);

                    let mut mac = master_mac.clone();

                    let header = format!(
                        r#"{{"msg_id":"{}","username":"jupyvim","session":"jupyvim-session","msg_type":"execute_request","version":"5.3"}}"#,
                        effective_msg_id
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
                        eprintln!("==> [Error] Failed to send message to ZMQ socket: {}", e);
                    } else {
                        println!("==> [Kernel-Actor] Execute request sent successfully with HMAC signature to Shell socket!");
                    }
                }
            }
        }
    }
}

impl KernelManager {
    /// Connects to ZMQ IOPub socket and subscribes to all topics
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
        
        println!("==> [Kernel] Connected to ZMQ IOPub socket at {}", endpoint);
        Ok(socket)
    }

    /// High-performance event loop for IOPub stream with zero-copy broadcast
    pub async fn run_iopub_actor(
        mut socket: zeromq::SubSocket,
        iopub_tx: broadcast::Sender<Arc<Vec<Bytes>>>,
    ) {
        // Already subscribed to all topics in connect_iopub(); this is the
        // consumer loop for that same socket, not a fresh connection.
        println!("==> [IOPub-Worker] Connected and ready for live stream.");

        // Fast I/O read loop
        loop {
            match socket.recv().await {
                Ok(zmq_msg) => {
                    let frames: Vec<Bytes> = zmq_msg.into_vec();
                    let shared_msg = Arc::new(frames);
                    let _ = iopub_tx.send(shared_msg);
                }
                Err(e) => {
                    eprintln!("==> [Error] Failed to receive message from IOPub socket: {}", e);
                    break;
                }
            }
        }
    }
}
