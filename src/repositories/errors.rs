//! Domain error type used by handlers and helpers.
//!
//! Implementing `ResponseError` lets handlers return `Result<HttpResponse,
//! ApiError>` and use `?` instead of repeating `match … return resp` blocks.

use actix_web::{http::StatusCode, HttpResponse, ResponseError};
use serde_json::{json, Value};
use std::fmt;

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub body: Value,
}

impl ApiError {
    pub fn new(status: StatusCode, body: Value) -> Self {
        Self { status, body }
    }

    fn simple(code: u16, error: &str) -> Self {
        Self::new(StatusCode::from_u16(code).unwrap(), json!({ "error": error }))
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.body)
    }
}

impl ResponseError for ApiError {
    fn status_code(&self) -> StatusCode {
        self.status
    }
    fn error_response(&self) -> HttpResponse {
        HttpResponse::build(self.status).json(&self.body)
    }
}

pub fn unauthorized(msg: &str) -> ApiError { ApiError::simple(401, msg) }
pub fn bad_request(msg: &str) -> ApiError { ApiError::simple(400, msg) }
pub fn forbidden_msg(msg: &str) -> ApiError { ApiError::simple(403, msg) }
pub fn forbidden() -> ApiError { forbidden_msg("forbidden") }
pub fn not_found(msg: &str) -> ApiError { ApiError::simple(404, msg) }
pub fn conflict(msg: &str) -> ApiError { ApiError::simple(409, msg) }
pub fn internal(msg: &str) -> ApiError { ApiError::simple(500, msg) }
