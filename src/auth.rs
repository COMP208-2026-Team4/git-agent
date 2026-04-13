use actix_web::HttpRequest;
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use std::env;

use crate::repositories::errors::{unauthorized, ApiError};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Subject - the user's snowflake ID
    pub sub: String,
    pub email: String,
    pub username: String,
    pub iat: Option<u64>,
    pub exp: Option<u64>,
}

/// Zero-trust JWT extraction and validation.
///
/// Reads the `Authorization: Bearer <token>` header, verifies the signature
/// against `JWT_SECRET`, and returns the decoded claims. Returns an
/// `ApiError` (which renders as a `401`) if the token is missing or invalid.
/// Non-failing auth: returns `Some(claims)` if a valid token is present,
/// `None` otherwise. Used by public-repo read endpoints.
pub fn optional_auth(req: &HttpRequest) -> Option<Claims> {
    let header = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())?;
    let token = header.strip_prefix("Bearer ")?;

    let secret = env::var("JWT_SECRET").unwrap_or_default();
    let key = DecodingKey::from_secret(secret.as_bytes());
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;

    decode::<Claims>(token, &key, &validation)
        .map(|data| data.claims)
        .ok()
}

pub fn require_auth(req: &HttpRequest) -> Result<Claims, ApiError> {
    let header = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| unauthorized("Missing Authorization header"))?;

    let token = header
        .strip_prefix("Bearer ")
        .ok_or_else(|| unauthorized("Malformed Authorization header"))?;

    let secret = env::var("JWT_SECRET").unwrap_or_else(|_| {
        eprintln!("[auth] JWT_SECRET is not set");
        String::new()
    });

    let key = DecodingKey::from_secret(secret.as_bytes());
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;

    decode::<Claims>(token, &key, &validation)
        .map(|data| data.claims)
        .map_err(|e| {
            eprintln!("[auth] JWT validation failed: {e}");
            unauthorized("Invalid or expired token")
        })
}
