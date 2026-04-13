//! Request/response DTOs for the repositories API.

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct CreateRepoRequest {
    pub name: String,
    pub user_id: String,
}

#[derive(Debug, Serialize)]
pub struct Repository {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub path: String,
    pub created_at: String,
    pub visibility: String,
    pub description: String,
    pub star_count: usize,
    pub updated_at: String,
}

#[derive(Debug, Deserialize)]
pub struct CommitsQuery {
    pub branch: Option<String>,
    pub limit: Option<u32>,
    pub page: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct TreeQuery {
    pub r#ref: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BlobQuery {
    pub r#ref: Option<String>,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct WriteFileBody {
    pub path: String,
    pub content: String,
    pub message: String,
    pub branch: String,
    pub author_name: String,
    pub author_email: String,
}

#[derive(Debug, Deserialize)]
pub struct DeleteFileBody {
    pub path: String,
    pub message: String,
    pub branch: String,
    pub author_name: String,
    pub author_email: String,
}

// ── Settings & collaboration DTOs ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct UpdateSettingsBody {
    pub visibility: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CollaboratorBody {
    pub user_id: String,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    pub r#type: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct PreviewQuery {
    pub branch: Option<String>,
}

