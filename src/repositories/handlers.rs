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

use super::authz::{require_authenticated_access, require_owner, require_read_access};
use super::errors::{
    bad_request, conflict, forbidden_msg, internal, not_found, ApiError,
};
use super::git::{path_exists, run_git, run_git_str, run_write_sequence, WriteUpdate};
use super::metadata::{read_meta, write_meta, RepoMeta};
use super::pathing::{is_safe_segment, repo_path, repos_root, user_dir};
use super::types::{
    BlobQuery, CollaboratorBody, CommitsQuery, CreateRepoRequest, DeleteFileBody,
    PreviewQuery, Repository, SearchQuery, TreeQuery,
    UpdateSettingsBody, WriteFileBody,
};

/// Convenience alias: every owner-scoped handler returns this.
type R = Result<HttpResponse, ApiError>;

/// True if the given branch ref resolves in the bare repo.
fn branch_exists(repo_dir: &str, branch: &str) -> bool {
    let branch_ref = format!("refs/heads/{branch}");
    Command::new("git")
        .args(["-C", repo_dir, "rev-parse", "--verify", &branch_ref])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Get the latest commit timestamp for a repo (epoch seconds).
fn latest_commit_timestamp(repo_dir: &str) -> Option<i64> {
    let out = Command::new("git")
        .args(["-C", repo_dir, "log", "-1", "--format=%at", "--all"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<i64>()
        .ok()
}

// ── Top-level repository endpoints ──────────────────────────────────────────

/// `GET /repositories` - lists all bare repositories owned by the caller.
pub async fn list_repositories(req: HttpRequest) -> R {
    let claims = require_auth(&req)?;
    let dir = user_dir(&claims.sub);

    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(HttpResponse::Ok().json(Vec::<Repository>::new())),
    };

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
        if !dir_name.ends_with(".git") {
            continue;
        }
        let name = dir_name.strip_suffix(".git").unwrap_or(&dir_name).to_string();
        let meta = read_meta(&claims.sub, &name);
        repos.push(Repository {
            id: Uuid::new_v4().to_string(),
            name,
            owner: owner_label.clone(),
            path: path.to_string_lossy().to_string(),
            created_at: meta.created_at.clone(),
            visibility: meta.visibility.clone(),
            description: meta.description.clone(),
            star_count: meta.stars.len(),
            updated_at: meta.updated_at.clone(),
        });
    }
    Ok(HttpResponse::Ok().json(repos))
}

/// `POST /repositories` - initialise a bare repository for the caller.
pub async fn create_repository(req: HttpRequest, body: web::Json<CreateRepoRequest>) -> R {
    let claims = require_auth(&req)?;

    let name = body.name.trim();
    if name.is_empty() {
        return Err(bad_request("name is required"));
    }
    if !is_safe_segment(name) {
        return Err(bad_request(
            "name may only contain letters, numbers, dashes, dots, and underscores",
        ));
    }

    if body.user_id != claims.sub {
        return Err(forbidden_msg(
            "user_id in body does not match the authenticated user",
        ));
    }

    if let Err(e) = fs::create_dir_all(user_dir(&claims.sub)) {
        eprintln!("[repos] Failed to create directory: {e}");
        return Err(internal("Failed to create repository directory"));
    }

    let path = repo_path(&claims.sub, name);
    let owner_label = if claims.username.is_empty() {
        claims.sub.clone()
    } else {
        claims.username.clone()
    };

    match Command::new("git").args(["init", "--bare", &path]).output() {
        Ok(o) if o.status.success() => {
            let now = Utc::now().to_rfc3339();
            let meta = RepoMeta {
                visibility: "public".to_string(),
                description: String::new(),
                stars: Vec::new(),
                collaborators: Vec::new(),
                created_at: now.clone(),
                updated_at: now.clone(),
            };
            let _ = write_meta(&claims.sub, name, &meta);

            Ok(HttpResponse::Created().json(Repository {
                id: Uuid::new_v4().to_string(),
                name: name.to_string(),
                owner: owner_label,
                path,
                created_at: now.clone(),
                visibility: "public".to_string(),
                description: String::new(),
                star_count: 0,
                updated_at: now,
            }))
        }
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

/// `DELETE /repositories/{owner}/{repo}` - permanently remove a repository.
pub async fn delete_repository(req: HttpRequest, path: web::Path<(String, String)>) -> R {
    let (owner, repo) = path.into_inner();
    // require_owner validates auth, ownership, and safe path segments
    let dir = require_owner(&req, &owner, &repo)?;

    // Derive canonical owner from auth claims (same logic as require_owner)
    let claims = require_auth(&req)?;
    let canonical = super::authz::ensure_owner(&owner, &claims)?;

    // Remove the bare repo directory
    match fs::remove_dir_all(&dir) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(not_found("repository not found"));
        }
        Err(e) => {
            eprintln!("[repos] Failed to remove repo directory: {e}");
            return Err(internal("failed to remove repository"));
        }
    }

    // Best-effort: remove the metadata sidecar (ignore errors if already gone)
    let _ = fs::remove_file(super::metadata::meta_path(&canonical, &repo));

    Ok(HttpResponse::NoContent().finish())
}

// ── Read endpoints (support public access) ──────────────────────────────────

/// `GET /repositories/{owner}/{repo}/branches`
pub async fn list_branches(req: HttpRequest, path: web::Path<(String, String)>) -> R {
    let (owner, repo) = path.into_inner();
    let (dir, _claims) = require_read_access(&req, &owner, &repo)?;

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
    let (dir, _claims) = require_read_access(&req, &owner, &repo)?;

    let branch = query.branch.clone().unwrap_or_else(|| "main".to_string());
    let limit = query.limit.unwrap_or(30).clamp(1, 100);
    let page = query.page.unwrap_or(1).max(1);
    let skip = (page - 1) * limit;
    let limit_arg = format!("-n{limit}");
    let skip_arg = format!("--skip={skip}");

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
    let (dir, _claims) = require_read_access(&req, &owner, &repo)?;

    let git_ref = query.r#ref.clone().unwrap_or_else(|| "HEAD".to_string());
    let sub_path = query.path.clone().unwrap_or_default();
    let spec = if sub_path.is_empty() {
        format!("{git_ref}:")
    } else {
        format!("{git_ref}:{}/", sub_path.trim_end_matches('/'))
    };

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
    let (dir, _claims) = require_read_access(&req, &owner, &repo)?;

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
    let (dir, _claims) = require_read_access(&req, &owner, &repo)?;
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

// ── Settings & metadata ─────────────────────────────────────────────────────

/// `GET /repositories/{owner}/{repo}/meta` - public metadata for a repo.
pub async fn get_repo_meta(req: HttpRequest, path: web::Path<(String, String)>) -> R {
    let (owner, repo) = path.into_inner();
    let (dir, claims) = require_read_access(&req, &owner, &repo)?;

    let canonical_owner = dir
        .strip_prefix(&format!("{}/", repos_root()))
        .and_then(|s| s.split('/').next())
        .unwrap_or(&owner);
    let meta = read_meta(canonical_owner, &repo);

    let starred_by_me = claims
        .as_ref()
        .map(|c| meta.stars.contains(&c.sub))
        .unwrap_or(false);

    Ok(HttpResponse::Ok().json(json!({
        "visibility": meta.visibility,
        "description": meta.description,
        "star_count": meta.stars.len(),
        "starred_by_me": starred_by_me,
        "collaborators": meta.collaborators,
        "created_at": meta.created_at,
        "updated_at": meta.updated_at,
    })))
}

/// `PUT /repositories/{owner}/{repo}/settings` - update visibility/description.
pub async fn update_settings(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    body: web::Json<UpdateSettingsBody>,
) -> R {
    let (owner, repo) = path.into_inner();
    let claims = require_auth(&req)?;
    if !is_safe_segment(&owner) || !is_safe_segment(&repo) {
        return Err(bad_request("invalid path segment"));
    }
    let canonical = super::authz::ensure_owner(&owner, &claims)?;
    let mut meta = read_meta(&canonical, &repo);

    if let Some(ref vis) = body.visibility {
        if vis != "public" && vis != "private" {
            return Err(bad_request("visibility must be 'public' or 'private'"));
        }
        meta.visibility = vis.clone();
    }
    if let Some(ref desc) = body.description {
        meta.description = desc.clone();
    }
    meta.updated_at = Utc::now().to_rfc3339();

    write_meta(&canonical, &repo, &meta)
        .map_err(|e| {
            eprintln!("[repos] failed to write metadata: {e}");
            internal("failed to save settings")
        })?;

    Ok(HttpResponse::Ok().json(json!({
        "visibility": meta.visibility,
        "description": meta.description,
    })))
}

// ── Stars ───────────────────────────────────────────────────────────────────

/// `POST /repositories/{owner}/{repo}/star`
pub async fn star_repo(req: HttpRequest, path: web::Path<(String, String)>) -> R {
    let (owner, repo) = path.into_inner();
    let (dir, claims) = require_authenticated_access(&req, &owner, &repo)?;

    let canonical_owner = dir
        .strip_prefix(&format!("{}/", repos_root()))
        .and_then(|s| s.split('/').next())
        .unwrap_or(&owner);

    let mut meta = read_meta(canonical_owner, &repo);
    if !meta.stars.contains(&claims.sub) {
        meta.stars.push(claims.sub.clone());
        meta.updated_at = Utc::now().to_rfc3339();
        write_meta(canonical_owner, &repo, &meta).map_err(|e| {
            eprintln!("[repos] failed to write metadata: {e}");
            internal("failed to save star")
        })?;
    }

    Ok(HttpResponse::Ok().json(json!({
        "starred": true,
        "star_count": meta.stars.len(),
    })))
}

/// `DELETE /repositories/{owner}/{repo}/star`
pub async fn unstar_repo(req: HttpRequest, path: web::Path<(String, String)>) -> R {
    let (owner, repo) = path.into_inner();
    let (dir, claims) = require_authenticated_access(&req, &owner, &repo)?;

    let canonical_owner = dir
        .strip_prefix(&format!("{}/", repos_root()))
        .and_then(|s| s.split('/').next())
        .unwrap_or(&owner);

    let mut meta = read_meta(canonical_owner, &repo);
    let before = meta.stars.len();
    meta.stars.retain(|s| s != &claims.sub);
    if meta.stars.len() != before {
        meta.updated_at = Utc::now().to_rfc3339();
        write_meta(canonical_owner, &repo, &meta).map_err(|e| {
            eprintln!("[repos] failed to write metadata: {e}");
            internal("failed to save star")
        })?;
    }

    Ok(HttpResponse::Ok().json(json!({
        "starred": false,
        "star_count": meta.stars.len(),
    })))
}

// ── Collaborators ───────────────────────────────────────────────────────────

/// `POST /repositories/{owner}/{repo}/collaborators`
pub async fn add_collaborator(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    body: web::Json<CollaboratorBody>,
) -> R {
    let (owner, repo) = path.into_inner();
    let claims = require_auth(&req)?;
    if !is_safe_segment(&owner) || !is_safe_segment(&repo) {
        return Err(bad_request("invalid path segment"));
    }
    let canonical = super::authz::ensure_owner(&owner, &claims)?;
    let mut meta = read_meta(&canonical, &repo);

    if meta.collaborators.contains(&body.user_id) {
        return Err(conflict("user is already a collaborator"));
    }

    meta.collaborators.push(body.user_id.clone());
    meta.updated_at = Utc::now().to_rfc3339();
    write_meta(&canonical, &repo, &meta).map_err(|e| {
        eprintln!("[repos] failed to write metadata: {e}");
        internal("failed to add collaborator")
    })?;

    Ok(HttpResponse::Ok().json(json!({
        "collaborators": meta.collaborators,
    })))
}

/// `DELETE /repositories/{owner}/{repo}/collaborators/{user_id}`
pub async fn remove_collaborator(
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
) -> R {
    let (owner, repo, user_id) = path.into_inner();
    let claims = require_auth(&req)?;
    if !is_safe_segment(&owner) || !is_safe_segment(&repo) {
        return Err(bad_request("invalid path segment"));
    }
    let canonical = super::authz::ensure_owner(&owner, &claims)?;
    let mut meta = read_meta(&canonical, &repo);

    let before = meta.collaborators.len();
    meta.collaborators.retain(|c| c != &user_id);
    if meta.collaborators.len() == before {
        return Err(not_found("user is not a collaborator"));
    }

    meta.updated_at = Utc::now().to_rfc3339();
    write_meta(&canonical, &repo, &meta).map_err(|e| {
        eprintln!("[repos] failed to write metadata: {e}");
        internal("failed to remove collaborator")
    })?;

    Ok(HttpResponse::Ok().json(json!({
        "collaborators": meta.collaborators,
    })))
}

// ── Search ──────────────────────────────────────────────────────────────────

/// `GET /search` - search repos and commits across all public repositories.
pub async fn search(req: HttpRequest, query: web::Query<SearchQuery>) -> R {
    let q = query.q.trim().to_lowercase();
    if q.is_empty() {
        return Err(bad_request("query is required"));
    }
    let limit = query.limit.unwrap_or(20).clamp(1, 50) as usize;
    let search_type = query.r#type.clone().unwrap_or_default();

    let claims = crate::auth::optional_auth(&req);
    let root = repos_root();
    let mut results = Vec::new();

    let user_dirs = match fs::read_dir(&root) {
        Ok(d) => d,
        Err(_) => return Ok(HttpResponse::Ok().json(json!({ "results": [] }))),
    };

    for user_entry in user_dirs.flatten() {
        if !user_entry.path().is_dir() {
            continue;
        }
        let uid = user_entry.file_name().to_string_lossy().to_string();

        if let Ok(repo_entries) = fs::read_dir(user_entry.path()) {
            for repo_entry in repo_entries.flatten() {
                let fname = repo_entry.file_name().to_string_lossy().to_string();
                if !fname.ends_with(".git") || !repo_entry.path().is_dir() {
                    continue;
                }
                let repo_name = fname.strip_suffix(".git").unwrap_or(&fname).to_string();
                let meta = read_meta(&uid, &repo_name);

                // Skip private repos unless caller is owner or collaborator
                if meta.visibility != "public" {
                    let allowed = claims.as_ref().map_or(false, |c| {
                        c.sub == uid || meta.collaborators.contains(&c.sub)
                    });
                    if !allowed {
                        continue;
                    }
                }

                // Search repos
                if search_type.is_empty() || search_type == "repo" {
                    if repo_name.to_lowercase().contains(&q)
                        || meta.description.to_lowercase().contains(&q)
                    {
                        results.push(json!({
                            "type": "repo",
                            "owner": uid,
                            "name": repo_name,
                            "description": meta.description,
                            "visibility": meta.visibility,
                            "star_count": meta.stars.len(),
                        }));
                    }
                }

                // Search commits
                if (search_type.is_empty() || search_type == "commit") && results.len() < limit {
                    let dir = repo_path(&uid, &repo_name);
                    if let Ok(log) = run_git_str(
                        &dir,
                        &["log", "--all", "--format=%H%x00%an%x00%s", "-n100"],
                        internal("search failed"),
                    ) {
                        for line in log.lines() {
                            let parts: Vec<&str> = line.splitn(3, '\u{0}').collect();
                            if parts.len() != 3 {
                                continue;
                            }
                            let sha = parts[0];
                            let author = parts[1];
                            let msg = parts[2];
                            if sha.to_lowercase().starts_with(&q)
                                || msg.to_lowercase().contains(&q)
                                || author.to_lowercase().contains(&q)
                            {
                                results.push(json!({
                                    "type": "commit",
                                    "owner": uid,
                                    "repo": repo_name,
                                    "sha": sha,
                                    "author": author,
                                    "message": msg,
                                }));
                            }
                            if results.len() >= limit {
                                break;
                            }
                        }
                    }
                }

                if results.len() >= limit {
                    break;
                }
            }
        }
        if results.len() >= limit {
            break;
        }
    }

    results.truncate(limit);
    Ok(HttpResponse::Ok().json(json!({ "results": results })))
}

// ── Profile repos (public listing for any user) ─────────────────────────────

/// `GET /repositories/profile/{owner}` - list repos visible to the caller.
pub async fn profile_repos(req: HttpRequest, path: web::Path<String>) -> R {
    let owner = path.into_inner();
    if !is_safe_segment(&owner) {
        return Err(bad_request("invalid owner"));
    }

    let claims = crate::auth::optional_auth(&req);
    let root = repos_root();

    // Resolve owner username to the canonical on-disk directory (snowflake ID).
    // When the authenticated caller is the owner we can use claims.sub directly.
    let is_self = claims.as_ref().map_or(false, |c| {
        c.sub == owner || c.username.eq_ignore_ascii_case(&owner)
    });
    let canonical = if is_self {
        claims.as_ref().map(|c| c.sub.clone()).unwrap_or_else(|| resolve_owner_dir(&root, &owner))
    } else {
        resolve_owner_dir(&root, &owner)
    };
    let dir = format!("{root}/{canonical}");

    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(HttpResponse::Ok().json(Vec::<serde_json::Value>::new())),
    };

    let mut repos = Vec::new();
    for entry in entries.flatten() {
        let fname = entry.file_name().to_string_lossy().to_string();
        if !fname.ends_with(".git") || !entry.path().is_dir() {
            continue;
        }
        let repo_name = fname.strip_suffix(".git").unwrap_or(&fname).to_string();
        let meta = read_meta(&canonical, &repo_name);

        // Only show private repos to the owner
        if meta.visibility != "public" && !is_self {
            continue;
        }

        let repo_dir = repo_path(&canonical, &repo_name);
        let last_ts = latest_commit_timestamp(&repo_dir);

        repos.push(json!({
            "name": repo_name,
            "owner": owner,
            "visibility": meta.visibility,
            "description": meta.description,
            "star_count": meta.stars.len(),
            "created_at": meta.created_at,
            "updated_at": meta.updated_at,
            "last_commit_timestamp": last_ts,
        }));
    }

    // Sort by last commit timestamp descending (most recently updated first)
    repos.sort_by(|a, b| {
        let ts_a = a["last_commit_timestamp"].as_i64().unwrap_or(0);
        let ts_b = b["last_commit_timestamp"].as_i64().unwrap_or(0);
        ts_b.cmp(&ts_a)
    });

    Ok(HttpResponse::Ok().json(repos))
}

fn resolve_owner_dir(root: &str, owner: &str) -> String {
    // Direct match
    let direct = format!("{root}/{owner}");
    if std::path::Path::new(&direct).is_dir() {
        return owner.to_string();
    }
    // Scan for case-insensitive match
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.eq_ignore_ascii_case(owner) {
                return name;
            }
        }
    }
    owner.to_string()
}

// ── Commit preview (branch exploration) ─────────────────────────────────────

/// `GET /repositories/{owner}/{repo}/commits/{sha}/preview`
/// Returns the tree state at the given commit for branch exploration.
pub async fn commit_preview(
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
    _query: web::Query<PreviewQuery>,
) -> R {
    let (owner, repo, sha) = path.into_inner();
    let (dir, _claims) = require_read_access(&req, &owner, &repo)?;

    if sha.is_empty() || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(bad_request("invalid sha"));
    }

    // Get commit info
    let commit_info = run_git_str(
        &dir,
        &["show", "--format=%H%x00%an%x00%ae%x00%at%x00%s", "-s", &sha],
        not_found("commit not found"),
    )?;

    let parts: Vec<&str> = commit_info.trim().splitn(5, '\u{0}').collect();
    let (commit_sha, author_name, author_email, timestamp_str, message) = if parts.len() == 5 {
        (parts[0], parts[1], parts[2], parts[3], parts[4])
    } else {
        (&*sha, "", "", "0", "")
    };

    // List branches that contain this commit
    let branches_out = run_git_str(
        &dir,
        &["branch", "--contains", &sha, "--format=%(refname:short)"],
        internal("git branch failed"),
    )
    .unwrap_or_default();
    let branches: Vec<&str> = branches_out
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();

    // Get the diff for this commit (summary)
    let diff_stat = run_git_str(
        &dir,
        &["diff-tree", "--stat", "--no-commit-id", "-r", &sha],
        internal("diff-tree failed"),
    )
    .unwrap_or_default();

    // Get the full diff
    let diff = run_git_str(
        &dir,
        &["diff-tree", "-p", "--no-commit-id", "-r", &sha],
        internal("diff-tree failed"),
    )
    .unwrap_or_default();

    // Get tree listing at this commit
    let tree_out = run_git_str(
        &dir,
        &["ls-tree", "--long", &format!("{sha}:")],
        internal("ls-tree failed"),
    )
    .unwrap_or_default();

    let tree_entries: Vec<_> = tree_out
        .lines()
        .filter_map(|line| {
            let mut head_and_name = line.splitn(2, '\t');
            let head = head_and_name.next().unwrap_or("");
            let name = head_and_name.next().unwrap_or("");
            let cols: Vec<&str> = head.split_whitespace().collect();
            (cols.len() >= 4).then(|| {
                json!({
                    "mode": cols[0],
                    "type": cols[1],
                    "sha": cols[2],
                    "size": if cols[3] == "-" { None } else { cols[3].parse::<u64>().ok() },
                    "name": name,
                })
            })
        })
        .collect();

    Ok(HttpResponse::Ok().json(json!({
        "sha": commit_sha,
        "author_name": author_name,
        "author_email": author_email,
        "timestamp": timestamp_str.parse::<i64>().unwrap_or(0),
        "message": message,
        "branches": branches,
        "diff_stat": diff_stat.trim(),
        "diff": diff,
        "tree": tree_entries,
    })))
}
