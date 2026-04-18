use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use std::path::PathBuf;
use std::process::Command;
use tokio::io::AsyncWriteExt;

/// The main orchestration engine for the ephemeral sandbox.
pub struct CoreEngine {
    pub cache_root: PathBuf,
}

impl CoreEngine {
    /// Initialize the engine, validating the existence of bubblewrap and required paths.
    pub fn new() -> Result<Self> {
        let project_dirs = directories::ProjectDirs::from("org", "arch-run", "arch-run")
            .ok_or_else(|| anyhow::anyhow!("Could not determine standard project directories for arch-run"))?;
        
        let cache_root = project_dirs.cache_dir().to_path_buf();
        std::fs::create_dir_all(&cache_root)
            .with_context(|| format!("Failed to ensure cache directory exists at {:?}", cache_root))?;

        Ok(Self { cache_root })
    }

    /// Resolves the dependency graph for a target package using `pacman -Sp`.
    /// Employs a non-cyclic directed graph (DAG) structure for resolution.
    pub async fn resolve_dependencies(&self, pkg_name: &str) -> Result<Vec<PackageLayer>> {
        // Execute pacman -Sp --print-format "%n %v %l" to get metadata without root.
        let output = Command::new("pacman")
            .args(["-Sp", "--print-format", "%n %v %l", pkg_name])
            .output()
            .with_context(|| format!("Failed to execute 'pacman' for package '{}'. Is pacman installed and accessible?", pkg_name))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("pacman resolution failed for '{}': {}", pkg_name, stderr.trim()));
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
                let response = client.get(&layer.url).send().await
                    .with_context(|| format!("Failed to initiate download for {} from {}", layer.name, layer.url))?;
                
                if !response.status().is_success() {
                    return Err(anyhow::anyhow!("Failed to download {} (status {}): {}", layer.name, response.status(), layer.url));
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
                
                // Flush and close the file before spawning the blocking task to unpack it
                file.flush().await?;
                drop(file);

                tokio::task::spawn_blocking(move || {
                    std::fs::create_dir_all(&layer.path)
                        .with_context(|| format!("Failed to create layer directory at {:?}", layer.path))?;
                    
                    let file = std::fs::File::open(&tmp_path)
                        .with_context(|| format!("Failed to open temporary file {:?}", tmp_path))?;
                        
                    let decoder = zstd::stream::read::Decoder::new(file)
                        .with_context(|| "Failed to initialize zstd decoder for package")?;
                        
                    let mut archive = tar::Archive::new(decoder);
                    archive.unpack(&layer.path)
                        .with_context(|| format!("Failed to unpack package {} into {:?}", layer.name, layer.path))?;
                        
                    std::fs::remove_file(&tmp_path)
                        .with_context(|| format!("Failed to cleanup temporary file {:?}", tmp_path))?;
                    Ok::<(), anyhow::Error>(())
                }).await.context("Layer extraction task panicked")??;

                Ok::<(), anyhow::Error>(())
            }));
        }

        for task in tasks {
            task.await??;
        }

        Ok(())
    }

    /// Builds a unified symlink farm (merged view) of all package layers.
    /// This prevents overlapping bind mount issues in bubblewrap and optimizes lookup performance.
    fn build_symlink_farm(&self, layers: &[PackageLayer]) -> Result<tempfile::TempDir> {
        let farm_dir = tempfile::tempdir_in(&self.cache_root)?;
        
        for layer in layers {
            let usr_path = layer.path.join("usr");
            if !usr_path.exists() {
                continue;
            }
            
            for entry in walkdir::WalkDir::new(&usr_path) {
                let entry = entry?;
                if let Ok(rel_path) = entry.path().strip_prefix(&usr_path) {
                    if rel_path.as_os_str().is_empty() {
                        continue;
                    }
                    
                    let target_path = farm_dir.path().join("usr").join(rel_path);
                    
                    if entry.file_type().is_dir() {
                        std::fs::create_dir_all(&target_path)
                            .with_context(|| format!("Failed to create directory in symlink farm: {:?}", target_path))?;
                    } else if !target_path.exists() {
                        std::os::unix::fs::symlink(entry.path(), &target_path)
                            .with_context(|| format!("Failed to create symlink: {:?} -> {:?}", target_path, entry.path()))?;
                    }
                }
            }
        }
        
        Ok(farm_dir)
    }

    /// Resolves the binary name to execute from a package.
    ///
    /// When a user provides `--bin`, validates it exists in a layer's `usr/bin/`.
    /// Otherwise scans the primary package layer for binaries:
    /// - If the package name matches a binary → use it (backward compat)
    /// - If only one binary exists → use it
    /// - If multiple binaries exist → error listing options
    /// - If no binaries found → error
    pub fn resolve_binary_name(
        &self,
        pkg_name: &str,
        layers: &[PackageLayer],
        explicit_bin: Option<&str>,
    ) -> Result<String> {
        // Collect all binaries across all layers (usr/bin/)
        let mut all_binaries: Vec<String> = Vec::new();
        let mut primary_binaries: Vec<String> = Vec::new();

        for layer in layers {
            let bin_dir = layer.path.join("usr").join("bin");
            if !bin_dir.exists() {
                continue;
            }

            let entries = std::fs::read_dir(&bin_dir)
                .with_context(|| format!("Failed to read bin directory: {:?}", bin_dir))?;

            for entry in entries {
                let entry = entry?;
                let file_type = entry.file_type()?;
                if !file_type.is_file() && !file_type.is_symlink() {
                    continue;
                }

                let name = entry.file_name().to_string_lossy().to_string();
                all_binaries.push(name.clone());

                if layer.name == pkg_name {
                    primary_binaries.push(name);
                }
            }
        }

        // Explicit --bin override
        if let Some(bin) = explicit_bin {
            if all_binaries.iter().any(|b| b == bin) {
                return Ok(bin.to_string());
            }

            let available = if all_binaries.is_empty() {
                "No binaries found in any layer".to_string()
            } else {
                format!("Available binaries: {}", all_binaries.join(", "))
            };

            return Err(anyhow::anyhow!(
                "Binary '{}' not found in package '{}'. {}",
                bin, pkg_name, available
            ));
        }

        // Backward compat: package name matches a binary
        if primary_binaries.iter().any(|b| b == pkg_name) {
            return Ok(pkg_name.to_string());
        }

        // Single binary → use it
        if primary_binaries.len() == 1 {
            return Ok(primary_binaries[0].clone());
        }

        // Multiple binaries → ambiguous
        if primary_binaries.len() > 1 {
            return Err(anyhow::anyhow!(
                "Multiple binaries found for package '{}': {}. Use --bin to select one.",
                pkg_name,
                primary_binaries.join(", ")
            ));
        }

        // No binaries in primary layer — fall back to all layers
        if all_binaries.len() == 1 {
            return Ok(all_binaries[0].clone());
        }

        if all_binaries.is_empty() {
            return Err(anyhow::anyhow!(
                "No binaries found for package '{}'. The package may not provide executables.",
                pkg_name
            ));
        }

        Err(anyhow::anyhow!(
            "No binaries found in primary package '{}'. Available in dependencies: {}. Use --bin to specify.",
            pkg_name,
            all_binaries.join(", ")
        ))
    }

    /// Spawns the bubblewrap container.
    /// Construct bwrap command-line arguments and execv.
    pub fn execute_sandbox(&self, entry_point: &str, layers: &[PackageLayer], args: &[String]) -> Result<()> {
        tracing::info!("Building unified symlink farm for layers...");
        let farm = self.build_symlink_farm(layers)?;
        
        let mut bwrap = Command::new("bwrap");
        
        bwrap.args(["--unshare-all", "--share-net", "--die-with-parent"]);

        // Mount the host filesystem read-only to provide glibc and other system libraries
        bwrap.args(["--ro-bind", "/", "/"]);

        // Basic mounts for a functional rootfs (must be after / to overlay it)
        bwrap.args(["--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp", "--tmpfs", "/dev/shm"]);

        // Bind the merged symlink farm into the sandbox at /tmp/arch-run
        let farm_path = farm.path().to_string_lossy().to_string();
        bwrap.args(["--dir", "/tmp/arch-run"]);
        bwrap.args(["--ro-bind", &farm_path, "/tmp/arch-run"]);

        // Allow read-write access to current working directory
        if let Ok(cwd) = std::env::current_dir() {
            let cwd_str = cwd.to_string_lossy().to_string();
            bwrap.args(["--bind", &cwd_str, &cwd_str]);
        }

        // Inject the package's bin and lib paths into the environment
        bwrap.arg("--setenv").arg("PATH").arg("/tmp/arch-run/usr/bin:/usr/local/bin:/usr/bin:/bin");
        bwrap.arg("--setenv").arg("LD_LIBRARY_PATH").arg("/tmp/arch-run/usr/lib:/tmp/arch-run/usr/lib64");

        // Execute the target
        bwrap.arg(entry_point);
        bwrap.args(args);

        let status = bwrap.status().with_context(|| format!("Failed to execute bwrap for entry point '{}'", entry_point))?;
        if !status.success() {
            return Err(anyhow::anyhow!("Sandbox execution failed for '{}' with exit status {}", entry_point, status));
        }

        Ok(())
    }
}

/// Command-line arguments for the arch-run executor.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None, arg_required_else_help = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    /// The name of the package to execute.
    package: Option<String>,

    /// Override the binary to execute within the package.
    #[arg(short, long)]
    bin: Option<String>,

    /// Arguments to pass to the executed package.
    #[arg(last = true)]
    args: Vec<String>,

    /// Increase logging verbosity (can be used multiple times).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
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
    let args = Args::parse();

    let level_filter = match args.verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level_filter));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(args.verbose > 1) // Only show targets (modules) in debug/trace
        .init();

    let engine = CoreEngine::new()?;

    match args.command {
        Some(Commands::Prune { all }) => {
            tracing::warn!("Pruning cache (all: {})", all);
            if engine.cache_root.exists() {
                if all {
                    std::fs::remove_dir_all(&engine.cache_root)
                        .with_context(|| format!("Failed to remove cache directory at {:?}", engine.cache_root))?;
                    std::fs::create_dir_all(&engine.cache_root)
                        .with_context(|| format!("Failed to recreate empty cache directory at {:?}", engine.cache_root))?;
                    println!("Cache completely pruned.");
                } else {
                    println!("Partial pruning not implemented. Use --all to clear completely.");
                }
            } else {
                println!("Cache does not exist at {:?}.", engine.cache_root);
            }
        }
        Some(Commands::List) => {
            println!("Cached Layers in {}:", engine.cache_root.display());
            if engine.cache_root.exists() {
                let mut count = 0;
                for entry in std::fs::read_dir(&engine.cache_root)
                    .with_context(|| format!("Failed to read cache directory at {:?}", engine.cache_root))? {
                    let entry = entry.context("Failed to read cache directory entry")?;
                    let name = entry.file_name().to_string_lossy().to_string();
                    let file_type = entry.file_type().context("Failed to get file type for cache entry")?;
                    if file_type.is_dir() && !name.starts_with(".tmp") {
                        println!("  - {}", name);
                        count += 1;
                    }
                }
                if count == 0 {
                    println!("  (Cache is empty)");
                }
            } else {
                println!("  (Cache directory not found at {:?})", engine.cache_root);
            }
        }
        None => {
            if let Some(pkg) = args.package {
                let layers = engine.resolve_dependencies(&pkg).await
                    .with_context(|| format!("Failed to resolve dependencies for package '{}'", pkg))?;
                engine.fetch_layers(&layers).await
                    .with_context(|| format!("Failed to fetch layers for package '{}'", pkg))?;
                let entry_point = engine.resolve_binary_name(&pkg, &layers, args.bin.as_deref())?;
                engine.execute_sandbox(&entry_point, &layers, &args.args)
                    .with_context(|| format!("Failed to execute sandbox for package '{}'", pkg))?;
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
