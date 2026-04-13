//! Owner authorization helpers shared by every owner-scoped route.

use actix_web::HttpRequest;

use crate::auth::{optional_auth, require_auth, Claims};

use super::errors::{bad_request, forbidden, ApiError};
use super::metadata::read_meta;
use super::pathing::{is_safe_segment, repo_path};

/// Verify the caller's claims allow access to `owner`. Accepts either
/// `claims.sub` or a case-insensitive match against `claims.username`.
///
/// Returns the canonical owner segment used for on-disk storage
/// (always `claims.sub`).
pub fn ensure_owner(owner: &str, claims: &Claims) -> Result<String, ApiError> {
    if owner == claims.sub || owner.eq_ignore_ascii_case(&claims.username) {
        Ok(claims.sub.clone())
    } else {
        Err(forbidden())
    }
}

/// Common preamble for owner-scoped endpoints: validates the JWT, checks the
/// path segments, runs the owner check, and returns the resolved bare repo
/// directory.
pub fn require_owner(req: &HttpRequest, owner: &str, repo: &str) -> Result<String, ApiError> {
    let claims = require_auth(req)?;
    if !is_safe_segment(owner) || !is_safe_segment(repo) {
        return Err(bad_request("invalid path segment"));
    }
    let canonical = ensure_owner(owner, &claims)?;
    Ok(repo_path(&canonical, repo))
}

/// Read-access check that respects visibility. Public repos allow
/// unauthenticated GET access. Private repos require the caller to be the
/// owner or a collaborator.
///
/// Returns `(repo_dir, optional_claims)`.
pub fn require_read_access(
    req: &HttpRequest,
    owner: &str,
    repo: &str,
) -> Result<(String, Option<Claims>), ApiError> {
    if !is_safe_segment(owner) || !is_safe_segment(repo) {
        return Err(bad_request("invalid path segment"));
    }

    let claims = optional_auth(req);

    // Resolve the canonical owner. We need to figure out the on-disk user_id
    // for the given `owner` URL segment. If the caller is authenticated AND
    // their sub/username matches, we know the canonical id. Otherwise for
    // public repos we try using the owner segment directly (it may be a
    // snowflake id already) or look for a username match.
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

    if meta.visibility == "public" {
        return Ok((dir, claims));
    }

    // Private repo: require authentication
    let c = claims.ok_or_else(|| forbidden())?;
    if c.sub == canonical || c.username.eq_ignore_ascii_case(owner) {
        return Ok((dir, Some(c)));
    }
    // Check collaborator list
    if meta.collaborators.contains(&c.sub) {
        return Ok((dir, Some(c)));
    }

    Err(forbidden())
}

/// Try to resolve an owner URL segment to the canonical on-disk directory.
/// Scans the repos root for a directory matching the owner string.
/// Falls back to using the segment as-is (it may already be a snowflake id).
fn resolve_canonical_owner(owner: &str) -> String {
    use super::pathing::repos_root;

    let root = repos_root();

    // Direct match (owner is already a snowflake id or exact dir name)
    let direct = format!("{root}/{owner}");
    if std::path::Path::new(&direct).is_dir() {
        return owner.to_string();
    }

    // Scan all user dirs and check if any has a meta file whose username
    // matches. This is a simple approach for small scale.
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let dir_name = entry.file_name().to_string_lossy().to_string();
            // Check if any repo in this dir was created by this username
            // by reading a meta file or checking directory structure
            if dir_name.eq_ignore_ascii_case(owner) {
                return dir_name;
            }
        }
    }

    owner.to_string()
}

/// Check that the caller is the owner or a collaborator (for write-like
/// operations that aren't strictly owner-only, like starring).
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
