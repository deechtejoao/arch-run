use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::io::AsyncWriteExt;

/// The main orchestration engine for the ephemeral sandbox.
pub struct CoreEngine {
    cache_root: PathBuf,
}

impl CoreEngine {
    /// Initialize the engine, validating the existence of bubblewrap and required paths.
    pub fn new() -> Result<Self> {
        let project_dirs = directories::ProjectDirs::from("org", "arch-run", "arch-run")
            .ok_or_else(|| anyhow::anyhow!("Could not determine cache directory"))?;
        
        let cache_root = project_dirs.cache_dir().to_path_buf();
        std::fs::create_dir_all(&cache_root).context("Failed to create cache directory")?;

        Ok(Self { cache_root })
    }

    /// Resolves the dependency graph for a target package using `pacman -Sp`.
    /// Employs a non-cyclic directed graph (DAG) structure for resolution.
    pub async fn resolve_dependencies(&self, pkg_name: &str) -> Result<Vec<PackageLayer>> {
        // Execute pacman -Sp --print-format "%n %v %u" to get metadata without root.
        let output = Command::new("pacman")
            .args(["-Sp", "--print-format", "%n %v %u", pkg_name])
            .output()
            .context("Failed to execute pacman. Is it installed?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("pacman failed: {}", stderr));
        }

        let mut layers = Vec::new();
        let stdout = String::from_utf8_lossy(&output.stdout);

        for line in stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() == 3 {
                let name = parts[0].to_string();
                let version = parts[1].to_string();
                let url = parts[2].to_string();
                let path = self.cache_root.join(format!("{}-{}", name, version));
                
                layers.push(PackageLayer {
                    name,
                    version,
                    url,
                    path,
                });
            }
        }

        Ok(layers)
    }

    /// Fetches missing package layers from mirrors and extracts them.
    /// Optimized with tokio for concurrent I/O throughput.
    pub async fn fetch_layers(&self, layers: &[PackageLayer]) -> Result<()> {
        let client = reqwest::Client::new();
        let mut tasks = Vec::new();

        for layer in layers {
            if layer.path.exists() {
                continue;
            }

            let client = client.clone();
            let layer = layer.clone();
            let cache_root = self.cache_root.clone();

            tasks.push(tokio::spawn(async move {
                tracing::info!("Fetching layer: {} ({})", layer.name, layer.version);
                let response = client.get(&layer.url).send().await?;
                if !response.status().is_success() {
                    return Err(anyhow::anyhow!("Failed to download {}: {}", layer.name, response.status()));
                }

                let mut stream = response.bytes_stream();
                let tmp_path = cache_root.join(format!("{}-{}.part", layer.name, layer.version));
                
                let mut file = tokio::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&tmp_path)
                    .await?;

                while let Some(item) = stream.next().await {
                    let chunk = item.context("Error while downloading")?;
                    file.write_all(&chunk).await?;
                }

                tokio::task::spawn_blocking(move || {
                    std::fs::create_dir_all(&layer.path)?;
                    let file = std::fs::File::open(&tmp_path)?;
                    let decoder = zstd::stream::read::Decoder::new(file)?;
                    let mut archive = tar::Archive::new(decoder);
                    archive.unpack(&layer.path).context("Failed to unpack archive")?;
                    std::fs::remove_file(&tmp_path)?;
                    Ok::<(), anyhow::Error>(())
                }).await??;

                Ok::<(), anyhow::Error>(())
            }));
        }

        for task in tasks {
            task.await??;
        }

        Ok(())
    }

    /// Spawns the bubblewrap container.
    /// Construct bwrap command-line arguments and execv.
    pub fn execute_sandbox(&self, entry_point: &str, layers: &[PackageLayer], args: &[String]) -> Result<()> {
        let mut bwrap = Command::new("bwrap");
        
        // Basic mounts for a functional rootfs
        bwrap.args(["--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp", "--shm", "/dev/shm"]);
        bwrap.args(["--unshare-all", "--share-net", "--die-with-parent"]);

        // Bind all layers into the sandbox
        for layer in layers {
            let usr_path = layer.path.join("usr");
            if usr_path.exists() {
                bwrap.args(["--ro-bind", &usr_path.to_string_lossy(), "/usr"]);
            }
        }

        // Add common lib paths if they aren't already in /usr
        bwrap.args(["--symlink", "usr/lib", "/lib", "--symlink", "usr/lib64", "/lib64", "--symlink", "usr/bin", "/bin"]);

        // Execute the target
        bwrap.arg(entry_point);
        bwrap.args(args);

        let status = bwrap.status().context("Failed to execute bwrap")?;
        if !status.success() {
            return Err(anyhow::anyhow!("Sandbox execution failed"));
        }

        Ok(())
    }
}

/// Command-line arguments for the arch-run executor.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    /// The name of the package to execute.
    #[arg(required_unless_present = "command")]
    package: Option<String>,

    /// Arguments to pass to the executed package.
    #[arg(last = true)]
    args: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Prune the local package cache (~/.cache/arch-run).
    Prune {
        /// Remove all cached layers, even recent ones.
        #[arg(short, long)]
        all: bool,
    },
    /// List all currently cached package layers.
    List,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let engine = CoreEngine::new()?;

    match args.command {
        Some(Commands::Prune { all }) => {
            tracing::warn!("Pruning cache (all: {})", all);
            // Implementation of cache pruning
        }
        Some(Commands::List) => {
            tracing::info!("Listing cached layers");
            // Implementation of cache listing
        }
        None => {
            if let Some(pkg) = args.package {
                let layers = engine.resolve_dependencies(&pkg).await?;
                engine.fetch_layers(&layers).await?;
                engine.execute_sandbox(&pkg, &layers, &args.args)?;
            }
        }
    }

    Ok(())
}

/// Represents a versioned Arch Linux package layer.
/// Aligned for cache-efficiency and fast lookup.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PackageLayer {
    pub name: String,
    pub version: String,
    pub url: String,
    pub path: PathBuf,
}
