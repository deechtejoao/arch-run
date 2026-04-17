use anyhow::{Context, Result};
use directories::{BaseDirs, ProjectDirs};
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Strongly-typed schema for our TOML configuration.
#[derive(Debug, Deserialize)]
pub struct CacheConfig {
    #[serde(default)]
    pub cache: CacheSection,
}

#[derive(Debug, Deserialize, Default)]
pub struct CacheSection {
    pub dir: Option<String>,
}

/// Resolves and safely provisions an application cache directory.
pub struct CacheManager {
    override_cli_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    app_name: &'static str,
}

impl CacheManager {
    /// Create a new CacheManager with standard precedence rules.
    pub fn new(app_name: &'static str, cli_dir: Option<PathBuf>, config_path: Option<PathBuf>) -> Self {
        Self {
            override_cli_dir: cli_dir,
            config_path,
            app_name,
        }
    }

    /// Resolves the priority chain and returns a fully provisioned, absolute cache directory path.
    pub fn init(&self) -> Result<PathBuf> {
        let dir = self.resolve_dir().context("Failed to resolve cache directory")?;
        let expanded = Self::expand_tilde(&dir).with_context(|| format!("Failed to expand tilde in path: {:?}", dir))?;

        if !expanded.exists() {
            fs::create_dir_all(&expanded)
                .with_context(|| format!("Failed to recursively create cache directory at {:?}", expanded))?;
        }

        Ok(expanded.canonicalize().with_context(|| format!("Failed to canonicalize cache path: {:?}", expanded)).unwrap_or(expanded))
    }

    /// Resolves the cache directory according to strict priority logic (highest to lowest):
    /// 1. CLI Arguments (`override_cli_dir`)
    /// 2. Environment Variables (`APP_CACHE_DIR`)
    /// 3. TOML configuration file (`config_path`)
    /// 4. System-specific defaults via XDG/AppData (`directories` crate)
    fn resolve_dir(&self) -> Result<PathBuf> {
        // Priority 1: CLI override
        if let Some(ref path) = self.override_cli_dir {
            return Ok(path.clone());
        }

        // Priority 2: Environment Variable
        if let Ok(env_path) = env::var("APP_CACHE_DIR") {
            return Ok(PathBuf::from(env_path));
        }

        // Priority 3: TOML Config File
        if let Some(ref path) = self.config_path {
            if path.exists() {
                let content = fs::read_to_string(path)
                    .with_context(|| format!("Failed to read configuration file at {:?}", path))?;
                    
                let config: CacheConfig = toml::from_str(&content)
                    .with_context(|| format!("Failed to parse TOML configuration from {:?}", path))?;
                    
                if let Some(dir) = config.cache.dir {
                    return Ok(PathBuf::from(dir));
                }
            }
        }

        // Priority 4: System defaults via 'directories' crate
        if let Some(proj_dirs) = ProjectDirs::from("", "", self.app_name) {
            return Ok(proj_dirs.cache_dir().to_path_buf());
        }

        anyhow::bail!(
            "Could not resolve a default system cache directory for app '{}'. \
             Please provide a directory via CLI, 'APP_CACHE_DIR' env var, or ensure system XDG paths are set.",
            self.app_name
        )
    }

    /// Expands `~` if present at the start of a path string safely.
    fn expand_tilde(path: &Path) -> Result<PathBuf> {
        if !path.starts_with("~") {
            return Ok(path.to_path_buf());
        }

        let base_dirs = BaseDirs::new().context("Failed to determine home directory when expanding `~`")?;
        let mut expanded = base_dirs.home_dir().to_path_buf();
        
        if let Ok(stripped) = path.strip_prefix("~") {
            expanded.push(stripped);
        }
        
        Ok(expanded)
    }
}
