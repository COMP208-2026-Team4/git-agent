//! JSON sidecar metadata for repositories.
//!
//! Each bare repo `{owner}/{name}.git` has a companion
//! `{owner}/{name}.meta.json` file that stores visibility, description,
//! star list, collaborator list, and timestamps.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;

use super::pathing::repos_root;

/// On-disk metadata for a single repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoMeta {
    pub visibility: String,
    pub description: String,
    pub stars: Vec<String>,
    pub collaborators: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl Default for RepoMeta {
    fn default() -> Self {
        let now = Utc::now().to_rfc3339();
        Self {
            visibility: "public".to_string(),
            description: String::new(),
            stars: Vec::new(),
            collaborators: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
        }
    }
}

/// Path to the metadata sidecar file for a given owner + repo name.
pub fn meta_path(owner: &str, name: &str) -> String {
    format!("{}/{owner}/{name}.meta.json", repos_root())
}

/// Read metadata from disk; returns defaults if the file doesn't exist.
pub fn read_meta(owner: &str, name: &str) -> RepoMeta {
    let path = meta_path(owner, name);
    match fs::read_to_string(&path) {
        Ok(json) => serde_json::from_str(&json).unwrap_or_default(),
        Err(_) => RepoMeta::default(),
    }
}

/// Write metadata to disk.
pub fn write_meta(owner: &str, name: &str, meta: &RepoMeta) -> std::io::Result<()> {
    let path = meta_path(owner, name);
    let json = serde_json::to_string_pretty(meta)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    fs::write(&path, json)
}

