use std::path::PathBuf;

/// Represents a versioned Arch Linux package layer.
/// Aligned for cache-efficiency and fast lookup.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PackageLayer {
    pub name: String,
    pub version: String,
    pub url: String,
    pub path: PathBuf,
}
