//! A high-performance, sandboxed I/O interaction layer between kernel-adjacent operations
//! and userspace.
//!
//! This implementation utilizes `tokio-uring` for asynchronous completion-queue based I/O on Linux,
//! falling back to standard `tokio` (epoll/kqueue) on other platforms. It implements a strict
//! Capability-based directory sandbox via `cap-std` to prohibit path traversal.

use async_trait::async_trait;
use bytes::Bytes;
use cap_std::fs::Dir;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;

/// The UserspaceDriver trait provides hooks for the userspace actor to handle completed I/O events.
/// It uses a message-passing architecture to bridge the gap between the I/O event loop and the consumer.
#[async_trait]
pub trait UserspaceDriver: Send + Sync + 'static {
    async fn on_read_complete(&self, path: &Path, data: Bytes) -> anyhow::Result<()>;
    async fn on_write_complete(&self, path: &Path, bytes_written: usize) -> anyhow::Result<()>;
}

/// Represents an I/O request initiated by the userspace actor.
pub enum IoRequest {
    Read {
        path: PathBuf,
    },
    Write {
        path: PathBuf,
        data: Vec<u8>,
    },
}

/// The SandboxContext struct encapsulates the capability-based directory handle and the channel
/// transmitter for sending requests to the isolated I/O loop.
pub struct SandboxContext {
    req_tx: mpsc::Sender<IoRequest>,
}

impl SandboxContext {
    /// Initializes a new SandboxContext.
    /// It spawns the isolated I/O loop in the background.
    pub fn start<P: AsRef<Path>>(
        root_path: P,
        driver: Arc<dyn UserspaceDriver>,
    ) -> anyhow::Result<Self> {
        let root_dir = Dir::open_ambient_dir(root_path, cap_std::ambient_authority())?;
        let root_dir = Arc::new(root_dir);

        let (req_tx, req_rx) = mpsc::channel(1024);

        // Spawn the architecture-specific I/O loop.
        #[cfg(target_os = "linux")]
        {
            std::thread::spawn(move || {
                tokio_uring::start(async move {
                    run_io_loop_linux(root_dir, req_rx, driver).await;
                });
            });
        }

        #[cfg(not(target_os = "linux"))]
        {
            tokio::spawn(async move {
                run_io_loop_standard(root_dir, req_rx, driver).await;
            });
        }

        Ok(Self { req_tx })
    }

    /// Submits a read request to the sandboxed I/O loop.
    pub async fn submit_read(&self, path: impl AsRef<Path>) -> anyhow::Result<()> {
        self.req_tx
            .send(IoRequest::Read {
                path: path.as_ref().to_path_buf(),
            })
            .await?;
        Ok(())
    }

    /// Submits a write request to the sandboxed I/O loop.
    pub async fn submit_write(&self, path: impl AsRef<Path>, data: Vec<u8>) -> anyhow::Result<()> {
        self.req_tx
            .send(IoRequest::Write {
                path: path.as_ref().to_path_buf(),
                data,
            })
            .await?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
async fn run_io_loop_linux(
    root_dir: Arc<Dir>,
    mut req_rx: mpsc::Receiver<IoRequest>,
    driver: Arc<dyn UserspaceDriver>,
) {
    while let Some(req) = req_rx.recv().await {
        let root_dir = root_dir.clone();
        let driver = driver.clone();

        tokio_uring::spawn(async move {
            match req {
                IoRequest::Read { path } => {
                    // capability-based resolution preventing traversal
                    if let Ok(cap_file) = root_dir.open(&path) {
                        let std_file = cap_file.into_std();
                        let uring_file = tokio_uring::fs::File::from_std(std_file);

                        let mut full_buf = Vec::new();
                        let mut offset = 0;
                        loop {
                            let buf = vec![0u8; 8192];
                            let (res, mut buf) = uring_file.read_at(buf, offset).await;
                            match res {
                                Ok(0) => break,
                                Ok(n) => {
                                    buf.truncate(n);
                                    full_buf.extend_from_slice(&buf);
                                    offset += n as u64;
                                }
                                Err(e) => {
                                    tracing::error!("Read error: {}", e);
                                    break;
                                }
                            }
                        }
                        let _ = driver.on_read_complete(&path, Bytes::from(full_buf)).await;
                    } else {
                        tracing::error!("Sandbox isolation blocked read access to {:?}", path);
                    }
                }
                IoRequest::Write { path, data } => {
                    // capability-based creation preventing traversal
                    if let Ok(cap_file) = root_dir.create(&path) {
                        let std_file = cap_file.into_std();
                        let uring_file = tokio_uring::fs::File::from_std(std_file);

                        let len = data.len();
                        let (res, _) = uring_file.write_at(data, 0).await;
                        match res {
                            Ok(n) => {
                                if n < len {
                                    tracing::warn!("Short write: {}/{} bytes", n, len);
                                }
                                let _ = driver.on_write_complete(&path, n).await;
                            }
                            Err(e) => tracing::error!("Write error: {}", e),
                        }
                    } else {
                        tracing::error!("Sandbox isolation blocked write access to {:?}", path);
                    }
                }
            }
        });
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_io_loop_standard(
    root_dir: Arc<Dir>,
    mut req_rx: mpsc::Receiver<IoRequest>,
    driver: Arc<dyn UserspaceDriver>,
) {
    while let Some(req) = req_rx.recv().await {
        let root_dir = root_dir.clone();
        let driver = driver.clone();

        tokio::spawn(async move {
            match req {
                IoRequest::Read { path } => {
                    if let Ok(cap_file) = root_dir.open(&path) {
                        let std_file = cap_file.into_std();
                        let mut tokio_file = tokio::fs::File::from_std(std_file);

                        let mut buf = Vec::new();
                        use tokio::io::AsyncReadExt;
                        if let Ok(_) = tokio_file.read_to_end(&mut buf).await {
                            let _ = driver.on_read_complete(&path, Bytes::from(buf)).await;
                        }
                    } else {
                        tracing::error!("Sandbox isolation blocked read access to {:?}", path);
                    }
                }
                IoRequest::Write { path, data } => {
                    if let Ok(cap_file) = root_dir.create(&path) {
                        let std_file = cap_file.into_std();
                        let mut tokio_file = tokio::fs::File::from_std(std_file);

                        use tokio::io::AsyncWriteExt;
                        if let Ok(_) = tokio_file.write_all(&data).await {
                            let _ = driver.on_write_complete(&path, data.len()).await;
                        }
                    } else {
                        tracing::error!("Sandbox isolation blocked write access to {:?}", path);
                    }
                }
            }
        });
    }
}

// ==============================================================================
// Demonstration / Entry Point
// ==============================================================================

/// A sample Userspace Actor demonstrating the integration of the SandboxContext.
struct ExampleActor;

#[async_trait]
impl UserspaceDriver for ExampleActor {
    async fn on_read_complete(&self, path: &Path, data: Bytes) -> anyhow::Result<()> {
        println!(
            "[Actor] Read completed for {:?}. Received {} bytes.",
            path,
            data.len()
        );
        Ok(())
    }

    async fn on_write_complete(&self, path: &Path, bytes_written: usize) -> anyhow::Result<()> {
        println!(
            "[Actor] Write completed for {:?}. Wrote {} bytes.",
            path, bytes_written
        );
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    
    // Create a temporary directory for the sandbox
    let temp_dir = tempfile::tempdir()?;
    let root_path = temp_dir.path().to_path_buf();
    
    println!("Initializing Capability-based Sandbox in {:?}", root_path);

    let actor = Arc::new(ExampleActor);
    
    // Start the highly isolated, io_uring-backed I/O context.
    let sandbox = SandboxContext::start(&root_path, actor)?;

    // Demonstration of safe writes
    println!("Submitting legitimate write request...");
    sandbox
        .submit_write(
            "secure_data.txt",
            b"Highly secure, sandboxed I/O transfer.".to_vec(),
        )
        .await?;

    // Allow some time for the async ring to complete
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Demonstration of safe reads
    println!("Submitting legitimate read request...");
    sandbox.submit_read("secure_data.txt").await?;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Demonstration of capability failure (Path Traversal Attempt)
    println!("Submitting illegal path traversal read attempt...");
    sandbox.submit_read("../../../etc/passwd").await?;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    println!("Demonstration complete.");
    Ok(())
}
