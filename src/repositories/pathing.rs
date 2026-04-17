//! Filesystem path derivation for bare repositories.
//!
//! Every on-disk path originates from `REPOS_DIR` (default `./repos`) &
//! the caller's canonical user id (`claims.sub`).

use std::env;

pub fn repos_root() -> String {
    env::var("REPOS_DIR").unwrap_or_else(|_| "./repos".to_string())
}

pub fn user_dir(owner: &str) -> String {
    format!("{}/{owner}", repos_root())
}

pub fn repo_path(owner: &str, name: &str) -> String {
    format!("{}/{owner}/{name}.git", repos_root())
}

/// Mirrors the character class accepted by `create_repository` for repo & owner
/// path segments. Summarily rejects empty strings.
pub fn is_safe_segment(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
}
