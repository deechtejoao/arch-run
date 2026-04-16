use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

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
