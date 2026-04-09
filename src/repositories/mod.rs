//! Repositories module - split into focused submodules:
//!
//! - [`errors`]  - domain `ApiError` type and HTTP error helpers
//! - [`pathing`] - bare-repo filesystem path resolution
//! - [`authz`]   - JWT/owner validation shared by every owner-scoped route
//! - [`git`]     - thin git CLI service and the write-sequence plumbing
//! - [`types`]   - request/response DTOs
//! - [`handlers`]- HTTP handlers (re-exported below for `main.rs`)

mod authz;
pub mod errors;
mod git;
mod handlers;
mod pathing;
mod types;

pub use handlers::{
    create_blob, create_repository, delete_blob, get_blob, get_diff, get_tree, list_branches,
    list_commits, list_repositories, update_blob,
};

#[cfg(test)]
mod tests;
