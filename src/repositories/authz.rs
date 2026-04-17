//! Proprietor-authorization utilities shared across every owner-scoped route.

use actix_web::HttpRequest;

use crate::auth::{optional_auth, require_auth, Claims};

use super::errors::{bad_request, forbidden, ApiError};
use super::metadata::read_meta;
use super::pathing::{is_safe_segment, repo_path};

/// Ascertain whether the caller's claims permit access to `owner`. Accepts either
/// `claims.sub` or a case-insensitive concordance with `claims.username`.
///
/// Yields the canonical owner segment employed for on-disk storage
/// (invariably `claims.sub`).
pub fn ensure_owner(owner: &str, claims: &Claims) -> Result<String, ApiError> {
    if owner == claims.sub || owner.eq_ignore_ascii_case(&claims.username) {
        Ok(claims.sub.clone())
    } else {
        Err(forbidden())
    }
}

/// Universal preamble for owner-scoped endpoints: scrutinises the JWT, inspects the
/// path segments, executes the owner verification, & returns the resolved bare repo
/// directory.
pub fn require_owner(req: &HttpRequest, owner: &str, repo: &str) -> Result<String, ApiError> {
    let claims = require_auth(req)?;
    if !is_safe_segment(owner) || !is_safe_segment(repo) {
        return Err(bad_request("invalid path segment"));
    }
    let canonical = ensure_owner(owner, &claims)?;
    Ok(repo_path(&canonical, repo))
}

/// Read-access check that honours visibility. Public repos permit
/// unauthenticated GET access. Private repos demand the caller be the
/// proprietor or a collaborator.
///
/// Yields `(repo_dir, optional_claims)`.
pub fn require_read_access(
    req: &HttpRequest,
    owner: &str,
    repo: &str,
) -> Result<(String, Option<Claims>), ApiError> {
    if !is_safe_segment(owner) || !is_safe_segment(repo) {
        return Err(bad_request("invalid path segment"));
    }

    let claims = optional_auth(req);

    // Resolve the canonical proprietor. We must ascertain the on-disk user_id
    // for the supplied `owner` URL segment. If the caller is authenticated &
    // their sub/username concurs, the canonical id is known. Otherwise for
    // public repos we attempt the owner segment directly (it may already be a
    // snowflake id) or probe for a username concordance.
    let canonical = if let Some(ref c) = claims {
        if owner == c.sub || owner.eq_ignore_ascii_case(&c.username) {
            c.sub.clone()
        } else {
            resolve_canonical_owner(owner)
        }
    } else {
        resolve_canonical_owner(owner)
    };

    let meta = read_meta(&canonical, repo);
    let dir = repo_path(&canonical, repo);

    if !std::path::Path::new(&dir).is_dir() {
        return Err(super::errors::not_found("repository not found"));
    }

    if meta.visibility == "public" {
        return Ok((dir, claims));
    }

    // Private repo - authentication is obligatory
    let c = claims.ok_or_else(|| forbidden())?;
    if c.sub == canonical || c.username.eq_ignore_ascii_case(owner) {
        return Ok((dir, Some(c)));
    }
    // Inspect the collaborator roster
    if meta.collaborators.contains(&c.sub) {
        return Ok((dir, Some(c)));
    }

    Err(forbidden())
}

/// Attempt to reconcile an owner URL segment with the canonical on-disk directory.
/// Traverses the repos root hunting for a directory matching the owner string.
/// Falls back gracefully to the segment as-is (it may already be a snowflake id).
fn resolve_canonical_owner(owner: &str) -> String {
    use super::pathing::repos_root;

    let root = repos_root();

    // Direct concordance (owner is already a snowflake id or precise dir name)
    let direct = format!("{root}/{owner}");
    if std::path::Path::new(&direct).is_dir() {
        return owner.to_string();
    }

    // Traverse all user dirs & check whether any holds a meta file whose username
    // concurs. A rudimentary approach - wholly adequate for modest scale.
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let dir_name = entry.file_name().to_string_lossy().to_string();
            // Determine whether any repo here was birthed under this username
            // by perusing a meta file or inspecting the directory structure
            if dir_name.eq_ignore_ascii_case(owner) {
                return dir_name;
            }
        }
    }

    owner.to_string()
}

/// Verify the caller is the proprietor or a collaborator (for write-adjacent
/// operations that aren't strictly owner-exclusive, such as starring).
pub fn require_authenticated_access(
    req: &HttpRequest,
    owner: &str,
    repo: &str,
) -> Result<(String, Claims), ApiError> {
    let claims = require_auth(req)?;
    if !is_safe_segment(owner) || !is_safe_segment(repo) {
        return Err(bad_request("invalid path segment"));
    }
    let canonical = if owner == claims.sub || owner.eq_ignore_ascii_case(&claims.username) {
        claims.sub.clone()
    } else {
        resolve_canonical_owner(owner)
    };
    let dir = repo_path(&canonical, repo);
    Ok((dir, claims))
}
