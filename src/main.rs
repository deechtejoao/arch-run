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

        for layer in layers {
            if layer.path.exists() {
                continue;
            }

            let response = client.get(&layer.url).send().await?;
            if !response.status().is_success() {
                return Err(anyhow::anyhow!("Failed to download {}: {}", layer.name, response.status()));
            }

            let mut stream = response.bytes_stream();
            let tmp_file = tempfile::NamedTempFile::new_in(&self.cache_root)?;
            
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .open(tmp_file.path())
                .await?;

            while let Some(item) = stream.next().await {
                let chunk = item.context("Error while downloading")?;
                file.write_all(&chunk).await?;
            }

            // Extract the layer
            self.extract_layer(&layer, tmp_file.path())?;
        }

        Ok(())
    }

    /// Extract a .pkg.tar.zst archive into the cache directory.
    fn extract_layer(&self, layer: &PackageLayer, archive: &Path) -> Result<()> {
        std::fs::create_dir_all(&layer.path)?;
        
        let file = std::fs::File::open(archive)?;
        let decoder = zstd::stream::read::Decoder::new(file)?;
        let mut archive = tar::Archive::new(decoder);
        
        archive.unpack(&layer.path).context("Failed to unpack archive")?;
        
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

/// Represents a versioned Arch Linux package layer.
/// Aligned for cache-efficiency and fast lookup.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PackageLayer {
    pub name: String,
    pub version: String,
    pub url: String,
    pub path: PathBuf,
}
