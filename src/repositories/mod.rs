//! Repositories module - split into focused submodules:
//!
//! - [`errors`]  - domain `ApiError` type and HTTP error helpers
//! - [`pathing`] - bare-repo filesystem path resolution
//! - [`authz`]   - JWT/owner validation shared by every owner-scoped route
//! - [`git`]     - thin git CLI service and the write-sequence plumbing
//! - [`metadata`]- JSON sidecar metadata (visibility, stars, collaborators)
//! - [`types`]   - request/response DTOs
//! - [`handlers`]- HTTP handlers (re-exported below for `main.rs`)

mod authz;
pub mod errors;
mod git;
mod handlers;
pub mod metadata;
mod pathing;
mod types;

pub use handlers::{
    create_blob, create_repository, delete_blob, get_blob, get_diff, get_tree, list_branches,
    list_commits, list_repositories, update_blob,
    // New handlers
    get_repo_meta, update_settings, star_repo, unstar_repo,
    add_collaborator, remove_collaborator,
    search, profile_repos, commit_preview,
};

#[cfg(test)]
mod tests;
