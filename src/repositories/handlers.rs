//! HTTP handlers for the repositories API.
//!
//! Handlers stay thin: they parse path/query/body, delegate to the helpers in
//! `authz`/`git`/`pathing`, and shape the JSON response. All errors flow
//! through `ApiError` and `?` instead of the previous `match … return resp`
//! ladders.

use actix_web::{http::StatusCode, web, HttpRequest, HttpResponse};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::Utc;
use serde_json::json;
use std::{fs, process::Command};
use uuid::Uuid;

use crate::auth::require_auth;

use super::authz::require_owner;
use super::errors::{
    bad_request, conflict, forbidden_msg, internal, not_found, ApiError,
};
use super::git::{path_exists, run_git, run_git_str, run_write_sequence, WriteUpdate};
use super::pathing::{is_safe_segment, repo_path, user_dir};
use super::types::{
    BlobQuery, CommitsQuery, CreateRepoRequest, DeleteFileBody, Repository, TreeQuery,
    WriteFileBody,
};

/// Convenience alias: every owner-scoped handler returns this.
type R = Result<HttpResponse, ApiError>;

/// True if the given branch ref resolves in the bare repo. Used by the
/// commits/tree handlers to short-circuit with an empty payload instead of
/// surfacing a 5xx for "branch doesn't exist yet" (which is the cold-load
/// failure mode the RepoPage hit before it knew the real default branch).
fn branch_exists(repo_dir: &str, branch: &str) -> bool {
    let branch_ref = format!("refs/heads/{branch}");
    Command::new("git")
        .args(["-C", repo_dir, "rev-parse", "--verify", &branch_ref])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ── Top-level repository endpoints ──────────────────────────────────────────

/// `GET /repositories` - lists all bare repositories owned by the caller.
pub async fn list_repositories(req: HttpRequest) -> R {
    let claims = require_auth(&req)?;
    let dir = user_dir(&claims.sub);

    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        // Directory doesn't exist yet - user has no repos.
        Err(_) => return Ok(HttpResponse::Ok().json(Vec::<Repository>::new())),
    };

    // Use the canonical username (not the snowflake `sub`) as the owner
    // segment so the frontend can build stable, human-readable URLs. The
    // on-disk path still lives under `sub` - we resolve that internally.
    let owner_label = if claims.username.is_empty() {
        claims.sub.clone()
    } else {
        claims.username.clone()
    };

    let mut repos = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let raw = entry.file_name();
        let dir_name = raw.to_string_lossy();
        let name = dir_name.strip_suffix(".git").unwrap_or(&dir_name).to_string();
        repos.push(Repository {
            id: Uuid::new_v4().to_string(),
            name,
            owner: owner_label.clone(),
            path: path.to_string_lossy().to_string(),
            created_at: String::new(),
        });
    }
    Ok(HttpResponse::Ok().json(repos))
}

/// `POST /repositories` - initialise a bare repository for the caller.
pub async fn create_repository(req: HttpRequest, body: web::Json<CreateRepoRequest>) -> R {
    // 1. Zero-trust: validate JWT before doing anything.
    let claims = require_auth(&req)?;

    // 2. Validate input.
    let name = body.name.trim();
    if name.is_empty() {
        return Err(bad_request("name is required"));
    }
    if !is_safe_segment(name) {
        return Err(bad_request(
            "name may only contain letters, numbers, dashes, dots, and underscores",
        ));
    }

    // 3. The user_id in the body must match the authenticated principal.
    if body.user_id != claims.sub {
        return Err(forbidden_msg(
            "user_id in body does not match the authenticated user",
        ));
    }

    // 4. Ensure the parent directory exists.
    if let Err(e) = fs::create_dir_all(user_dir(&claims.sub)) {
        eprintln!("[repos] Failed to create directory: {e}");
        return Err(internal("Failed to create repository directory"));
    }

    // 5. Initialise a bare repository using the git CLI.
    let path = repo_path(&claims.sub, name);
    let owner_label = if claims.username.is_empty() {
        claims.sub.clone()
    } else {
        claims.username.clone()
    };
    match Command::new("git").args(["init", "--bare", &path]).output() {
        Ok(o) if o.status.success() => Ok(HttpResponse::Created().json(Repository {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            owner: owner_label,
            path,
            created_at: Utc::now().to_rfc3339(),
        })),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            eprintln!("[repos] git init failed: {stderr}");
            Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": "git init failed", "detail": stderr.trim() }),
            ))
        }
        Err(e) => {
            eprintln!("[repos] Failed to run git: {e}");
            Err(internal("Failed to run git command"))
        }
    }
}

// ── Read endpoints ──────────────────────────────────────────────────────────

/// `GET /repositories/{owner}/{repo}/branches`
pub async fn list_branches(req: HttpRequest, path: web::Path<(String, String)>) -> R {
    let (owner, repo) = path.into_inner();
    let dir = require_owner(&req, &owner, &repo)?;

    let stdout = run_git_str(
        &dir,
        &["branch", "-a", "--format=%(refname:short)"],
        internal("git branch failed"),
    )?;

    let mut branches: Vec<String> = Vec::new();
    for line in stdout.lines() {
        let name = line.trim();
        if name.is_empty() || name == "HEAD" || name.ends_with("/HEAD") {
            continue;
        }
        let stripped = name.strip_prefix("origin/").unwrap_or(name).to_string();
        if !branches.contains(&stripped) {
            branches.push(stripped);
        }
    }

    // Resolve the symbolic HEAD so the frontend can pick a sensible default
    // branch when navigating to a repo cold (eliminating the "branch=main"
    // hardcode that 500'd repos with a different default).
    let head = run_git_str(
        &dir,
        &["symbolic-ref", "--short", "HEAD"],
        internal("git symbolic-ref failed"),
    )
    .map(|s| s.trim().to_string())
    .ok()
    .filter(|s| !s.is_empty());

    Ok(HttpResponse::Ok().json(json!({ "branches": branches, "head": head })))
}

/// `GET /repositories/{owner}/{repo}/commits`
pub async fn list_commits(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    query: web::Query<CommitsQuery>,
) -> R {
    let (owner, repo) = path.into_inner();
    let dir = require_owner(&req, &owner, &repo)?;

    let branch = query.branch.clone().unwrap_or_else(|| "main".to_string());
    let limit = query.limit.unwrap_or(30).clamp(1, 100);
    let page = query.page.unwrap_or(1).max(1);
    let skip = (page - 1) * limit;
    let limit_arg = format!("-n{limit}");
    let skip_arg = format!("--skip={skip}");

    // If the branch ref doesn't exist (e.g. fresh repo or different default
    // branch), return an empty list rather than a 500. This stops the cold
    // RepoPage load from showing a "git log failed" error before the
    // frontend has had a chance to discover the real default branch.
    if !branch_exists(&dir, &branch) {
        return Ok(HttpResponse::Ok().json(json!({ "commits": [] })));
    }

    let stdout = run_git_str(
        &dir,
        &[
            "log",
            &branch,
            "--format=%H%x00%an%x00%ae%x00%at%x00%s",
            &limit_arg,
            &skip_arg,
        ],
        internal("git log failed"),
    )?;

    let commits: Vec<_> = stdout
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(5, '\u{0}').collect();
            (parts.len() == 5).then(|| {
                json!({
                    "sha": parts[0],
                    "author_name": parts[1],
                    "author_email": parts[2],
                    "timestamp": parts[3].parse::<i64>().unwrap_or(0),
                    "message": parts[4],
                })
            })
        })
        .collect();

    Ok(HttpResponse::Ok().json(json!({ "commits": commits })))
}

/// `GET /repositories/{owner}/{repo}/tree`
pub async fn get_tree(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    query: web::Query<TreeQuery>,
) -> R {
    let (owner, repo) = path.into_inner();
    let dir = require_owner(&req, &owner, &repo)?;

    let git_ref = query.r#ref.clone().unwrap_or_else(|| "HEAD".to_string());
    let sub_path = query.path.clone().unwrap_or_default();
    let spec = if sub_path.is_empty() {
        format!("{git_ref}:")
    } else {
        format!("{git_ref}:{}/", sub_path.trim_end_matches('/'))
    };

    // Empty repo / unknown branch: return an empty tree instead of 404 so the
    // RepoPage can render its branch selector and resolve the real default.
    if git_ref != "HEAD" && !branch_exists(&dir, &git_ref) {
        return Ok(HttpResponse::Ok().json(json!({
            "ref": git_ref,
            "path": sub_path,
            "entries": [],
        })));
    }

    let stdout = run_git_str(&dir, &["ls-tree", "--long", &spec], not_found("tree not found"))?;

    let entries: Vec<_> = stdout
        .lines()
        .filter_map(|line| {
            let mut head_and_name = line.splitn(2, '\t');
            let head = head_and_name.next().unwrap_or("");
            let name = head_and_name.next().unwrap_or("");
            let cols: Vec<&str> = head.split_whitespace().collect();
            (cols.len() >= 4).then(|| {
                let size = if cols[3] == "-" {
                    None
                } else {
                    cols[3].parse::<u64>().ok()
                };
                json!({
                    "mode": cols[0],
                    "type": cols[1],
                    "sha": cols[2],
                    "size": size,
                    "name": name,
                })
            })
        })
        .collect();

    Ok(HttpResponse::Ok().json(json!({
        "ref": git_ref,
        "path": sub_path,
        "entries": entries,
    })))
}

/// `GET /repositories/{owner}/{repo}/blob`
pub async fn get_blob(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    query: web::Query<BlobQuery>,
) -> R {
    let (owner, repo) = path.into_inner();
    let dir = require_owner(&req, &owner, &repo)?;

    let git_ref = query.r#ref.clone().unwrap_or_else(|| "HEAD".to_string());
    let spec = format!("{}:{}", git_ref, query.path);

    let bytes = run_git(&dir, &["show", &spec], not_found("file not found"))?;

    let scan_len = bytes.len().min(8000);
    let is_binary = bytes[..scan_len].contains(&0u8);
    let size = bytes.len();
    let encoded = BASE64.encode(&bytes);

    Ok(HttpResponse::Ok().json(json!({
        "ref": git_ref,
        "path": query.path,
        "content": encoded,
        "is_binary": is_binary,
        "size": size,
    })))
}

/// `GET /repositories/{owner}/{repo}/commits/{sha}/diff`
pub async fn get_diff(req: HttpRequest, path: web::Path<(String, String, String)>) -> R {
    let (owner, repo, sha) = path.into_inner();
    let dir = require_owner(&req, &owner, &repo)?;
    if sha.is_empty() || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(bad_request("invalid sha"));
    }

    let stdout_bytes = run_git(
        &dir,
        &[
            "show",
            "--format=%H%x00%an%x00%ae%x00%at%x00%s%x00",
            "--stat",
            "-p",
            &sha,
        ],
        not_found("commit not found"),
    )?;
    let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();

    let mut parts = stdout.splitn(6, '\u{0}');
    let header_sha = parts.next().unwrap_or("").to_string();
    let author_name = parts.next().unwrap_or("").to_string();
    let author_email = parts.next().unwrap_or("").to_string();
    let timestamp = parts.next().unwrap_or("0").parse::<i64>().unwrap_or(0);
    let message = parts.next().unwrap_or("").to_string();
    let rest = parts.next().unwrap_or("");

    let diff_idx = rest.find("\ndiff --git").map(|i| i + 1);
    let (stat_section, diff_text) = match diff_idx {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };

    let (mut files_changed, mut insertions, mut deletions) = (0u32, 0u32, 0u32);
    for line in stat_section.lines() {
        let trimmed = line.trim();
        if !(trimmed.contains("file changed") || trimmed.contains("files changed")) {
            continue;
        }
        for chunk in trimmed.split(',') {
            let chunk = chunk.trim();
            if let Some(num_str) = chunk.split_whitespace().next() {
                let n: u32 = num_str.parse().unwrap_or(0);
                if chunk.contains("file") {
                    files_changed = n;
                } else if chunk.contains("insertion") {
                    insertions = n;
                } else if chunk.contains("deletion") {
                    deletions = n;
                }
            }
        }
    }

    Ok(HttpResponse::Ok().json(json!({
        "sha": header_sha,
        "author_name": author_name,
        "author_email": author_email,
        "timestamp": timestamp,
        "message": message,
        "stats": {
            "files_changed": files_changed,
            "insertions": insertions,
            "deletions": deletions,
        },
        "diff": diff_text,
    })))
}

// ── Write endpoints ─────────────────────────────────────────────────────────

fn decode_b64(s: &str) -> Result<Vec<u8>, ApiError> {
    BASE64
        .decode(s.as_bytes())
        .map_err(|_| bad_request("content is not valid base64"))
}

fn write_response(status: StatusCode, sha: String, path: &str, branch: &str) -> HttpResponse {
    HttpResponse::build(status).json(json!({
        "sha": sha,
        "path": path,
        "branch": branch,
    }))
}

/// `POST /repositories/{owner}/{repo}/blob` - create a new file. 409 on conflict.
pub async fn create_blob(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    body: web::Json<WriteFileBody>,
) -> R {
    let (owner, repo) = path.into_inner();
    let dir = require_owner(&req, &owner, &repo)?;

    if body.path.trim().is_empty() {
        return Err(bad_request("path is required"));
    }
    if path_exists(&dir, &body.branch, &body.path) {
        return Err(conflict("file already exists"));
    }
    let content = decode_b64(&body.content)?;

    let sha = run_write_sequence(
        &dir,
        &body.branch,
        &body.message,
        &body.author_name,
        &body.author_email,
        WriteUpdate::Add {
            path: body.path.clone(),
            content,
        },
    )?;
    Ok(write_response(StatusCode::CREATED, sha, &body.path, &body.branch))
}

/// `PUT /repositories/{owner}/{repo}/blob` - overwrite an existing file.
pub async fn update_blob(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    body: web::Json<WriteFileBody>,
) -> R {
    let (owner, repo) = path.into_inner();
    let dir = require_owner(&req, &owner, &repo)?;

    if !path_exists(&dir, &body.branch, &body.path) {
        return Err(not_found("file not found"));
    }
    let content = decode_b64(&body.content)?;

    let sha = run_write_sequence(
        &dir,
        &body.branch,
        &body.message,
        &body.author_name,
        &body.author_email,
        WriteUpdate::Add {
            path: body.path.clone(),
            content,
        },
    )?;
    Ok(write_response(StatusCode::OK, sha, &body.path, &body.branch))
}

/// `DELETE /repositories/{owner}/{repo}/blob` - remove a file and commit.
pub async fn delete_blob(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    body: web::Json<DeleteFileBody>,
) -> R {
    let (owner, repo) = path.into_inner();
    let dir = require_owner(&req, &owner, &repo)?;

    if !path_exists(&dir, &body.branch, &body.path) {
        return Err(not_found("file not found"));
    }

    let sha = run_write_sequence(
        &dir,
        &body.branch,
        &body.message,
        &body.author_name,
        &body.author_email,
        WriteUpdate::Remove {
            path: body.path.clone(),
        },
    )?;
    Ok(write_response(StatusCode::OK, sha, &body.path, &body.branch))
}
