use actix_web::{HttpRequest, HttpResponse};
use jsonwebtoken::{decode, DecodingKey, Validation, Algorithm};
use serde::{Deserialize, Serialize};
use std::env;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Subject — the user's snowflake ID
    pub sub: String,
    pub email: String,
    pub username: String,
    pub iat: Option<u64>,
    pub exp: Option<u64>,
}

/// Zero-trust JWT extraction and validation.
///
/// Reads the `Authorization: Bearer <token>` header, verifies the
/// signature against `JWT_SECRET`, and returns the decoded claims.
/// Returns `Err(HttpResponse)` with the appropriate status code if
/// the token is missing or invalid.
pub fn require_auth(req: &HttpRequest) -> Result<Claims, HttpResponse> {
    let header = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            HttpResponse::Unauthorized()
                .json(serde_json::json!({"error": "Missing Authorization header"}))
        })?;

    let token = header.strip_prefix("Bearer ").ok_or_else(|| {
        HttpResponse::Unauthorized()
            .json(serde_json::json!({"error": "Malformed Authorization header"}))
    })?;

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
            HttpResponse::Unauthorized()
                .json(serde_json::json!({"error": "Invalid or expired token"}))
        })
}
