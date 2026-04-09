//! Owner authorization helpers shared by every owner-scoped route.

use actix_web::HttpRequest;

use crate::auth::{require_auth, Claims};

use super::errors::{bad_request, forbidden, ApiError};
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
