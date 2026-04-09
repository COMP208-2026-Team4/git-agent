use actix_web::{web, HttpRequest, HttpResponse};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, env, process::Command, sync::Mutex};
use uuid::Uuid;

use crate::auth::require_auth;

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Resolve the on-disk path of a bare repository for a given owner/name.
fn repo_path(owner: &str, name: &str) -> String {
    let root = env::var("REPOS_DIR").unwrap_or_else(|_| "./repos".to_string());
    format!("{root}/{owner}/{name}.git")
}

/// Validate that an owner/repo path segment matches the same character class
/// allowed by `create_repository`. Returns `false` for empty strings.
fn is_safe_segment(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Verify the caller's claims allow access to `owner`. Returns an HTTP error
/// response if the path owner does not match `claims.sub`.
fn ensure_owner(owner: &str, claims_sub: &str) -> Result<(), HttpResponse> {
    if owner == claims_sub {
        Ok(())
    } else {
        Err(HttpResponse::Forbidden()
            .json(serde_json::json!({"error": "forbidden"})))
    }
}

/// Per-repository write lock used to serialise plumbing-based file writes so
/// the bare repository's index file is never read/modified by two writers
/// concurrently.
fn write_lock(repo: &str) -> std::sync::Arc<Mutex<()>> {
    use std::sync::{Arc, OnceLock};
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let map = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap();
    guard
        .entry(repo.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

// ── Types ────────────────────────────────────────────────────────────────────

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
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// `GET /repositories`
///
/// Lists all bare Git repositories owned by the authenticated user.
pub async fn list_repositories(req: HttpRequest) -> HttpResponse {
    let claims = match require_auth(&req) {
        Ok(c) => c,
        Err(resp) => return resp,
    };

    let repos_root = env::var("REPOS_DIR").unwrap_or_else(|_| "./repos".to_string());
    let user_dir = format!("{repos_root}/{}", claims.sub);

    let entries = match fs::read_dir(&user_dir) {
        Ok(e) => e,
        Err(_) => {
            // Directory doesn't exist yet — user has no repos
            return HttpResponse::Ok().json(Vec::<Repository>::new());
        }
    };

    let mut repos: Vec<Repository> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let raw_name = entry.file_name();
            let dir_name = raw_name.to_string_lossy();
            // Strip the ".git" suffix that bare repos have
            let name = dir_name.strip_suffix(".git").unwrap_or(&dir_name).to_string();
            repos.push(Repository {
                id: Uuid::new_v4().to_string(),
                name,
                owner: claims.sub.clone(),
                path: path.to_string_lossy().to_string(),
                created_at: String::new(),
            });
        }
    }

    HttpResponse::Ok().json(repos)
}

/// `POST /repositories`
///
/// Creates a bare Git repository on disk. The caller must supply a valid JWT
/// (zero-trust — we never trust forwarded headers alone).
///
/// Request body:
/// ```json
/// { "name": "my-repo", "user_id": "1234567890" }
/// ```
pub async fn create_repository(
    req: HttpRequest,
    body: web::Json<CreateRepoRequest>,
) -> HttpResponse {
    // 1. Zero-trust: validate JWT before doing anything
    let claims = match require_auth(&req) {
        Ok(c) => c,
        Err(resp) => return resp,
    };

    // 2. Validate input
    let name = body.name.trim();
    if name.is_empty() {
        return HttpResponse::BadRequest()
            .json(serde_json::json!({"error": "name is required"}));
    }
    // Allow only safe characters in repo names
    if !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.') {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "name may only contain letters, numbers, dashes, dots, and underscores"
        }));
    }

    // 3. The user_id in the body must match the authenticated principal
    if body.user_id != claims.sub {
        return HttpResponse::Forbidden().json(serde_json::json!({
            "error": "user_id in body does not match the authenticated user"
        }));
    }

    // 4. Resolve the storage directory
    let repos_root = env::var("REPOS_DIR").unwrap_or_else(|_| "./repos".to_string());
    let repo_path = format!("{repos_root}/{}/{}.git", claims.sub, name);

    // 5. Create parent directories if needed
    if let Err(e) = std::fs::create_dir_all(format!("{repos_root}/{}", claims.sub)) {
        eprintln!("[repos] Failed to create directory: {e}");
        return HttpResponse::InternalServerError()
            .json(serde_json::json!({"error": "Failed to create repository directory"}));
    }

    // 6. Initialise a bare repository using the git CLI
    let output = Command::new("git")
        .args(["init", "--bare", &repo_path])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let repo = Repository {
                id: Uuid::new_v4().to_string(),
                name: name.to_string(),
                owner: claims.sub.clone(),
                path: repo_path,
                created_at: Utc::now().to_rfc3339(),
            };
            HttpResponse::Created().json(repo)
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            eprintln!("[repos] git init failed: {stderr}");
            HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "git init failed", "detail": stderr.trim()}))
        }
        Err(e) => {
            eprintln!("[repos] Failed to run git: {e}");
            HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Failed to run git command"}))
        }
    }
}

// ── Read endpoints ───────────────────────────────────────────────────────────

/// Common preamble for the new owner-scoped endpoints. Performs JWT validation,
/// path-segment validation, and the owner equality check in one place.
fn require_owner(
    req: &HttpRequest,
    owner: &str,
    repo: &str,
) -> Result<String, HttpResponse> {
    let claims = require_auth(req)?;
    if !is_safe_segment(owner) || !is_safe_segment(repo) {
        return Err(HttpResponse::BadRequest()
            .json(serde_json::json!({"error": "invalid path segment"})));
    }
    ensure_owner(owner, &claims.sub)?;
    Ok(repo_path(owner, repo))
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

/// `GET /repositories/{owner}/{repo}/branches`
///
/// Returns the deduplicated set of local and remote-tracking branches in the
/// repository. Remote-tracking refs have their `origin/` prefix stripped.
pub async fn list_branches(
    req: HttpRequest,
    path: web::Path<(String, String)>,
) -> HttpResponse {
    let (owner, repo) = path.into_inner();
    let repo_dir = match require_owner(&req, &owner, &repo) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let output = Command::new("git")
        .args(["-C", &repo_dir, "branch", "-a", "--format=%(refname:short)"])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
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
            HttpResponse::Ok().json(serde_json::json!({ "branches": branches }))
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            eprintln!("[repos] git branch failed: {stderr}");
            HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "git branch failed"}))
        }
        Err(e) => {
            eprintln!("[repos] Failed to run git: {e}");
            HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Failed to run git command"}))
        }
    }
}

/// `GET /repositories/{owner}/{repo}/commits`
///
/// Lists commits on a branch with pagination support. The `limit` query
/// parameter defaults to 30 and is capped at 100; `page` is 1-indexed.
pub async fn list_commits(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    query: web::Query<CommitsQuery>,
) -> HttpResponse {
    let (owner, repo) = path.into_inner();
    let repo_dir = match require_owner(&req, &owner, &repo) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let branch = query.branch.clone().unwrap_or_else(|| "main".to_string());
    let limit = query.limit.unwrap_or(30).min(100).max(1);
    let page = query.page.unwrap_or(1).max(1);
    let skip = (page - 1) * limit;

    let limit_arg = format!("-n{limit}");
    let skip_arg = format!("--skip={skip}");
    let format_arg = "--format=%H%x00%an%x00%ae%x00%at%x00%s".to_string();

    let output = Command::new("git")
        .args([
            "-C", &repo_dir, "log", &branch,
            &format_arg, &limit_arg, &skip_arg,
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let mut commits = Vec::new();
            for line in stdout.lines() {
                let parts: Vec<&str> = line.splitn(5, '\u{0}').collect();
                if parts.len() == 5 {
                    commits.push(serde_json::json!({
                        "sha": parts[0],
                        "author_name": parts[1],
                        "author_email": parts[2],
                        "timestamp": parts[3].parse::<i64>().unwrap_or(0),
                        "message": parts[4],
                    }));
                }
            }
            HttpResponse::Ok().json(serde_json::json!({ "commits": commits }))
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            eprintln!("[repos] git log failed: {stderr}");
            HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "git log failed"}))
        }
        Err(e) => {
            eprintln!("[repos] Failed to run git: {e}");
            HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Failed to run git command"}))
        }
    }
}

/// `GET /repositories/{owner}/{repo}/tree`
///
/// Returns one level of the repository tree at the given ref and path.
pub async fn get_tree(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    query: web::Query<TreeQuery>,
) -> HttpResponse {
    let (owner, repo) = path.into_inner();
    let repo_dir = match require_owner(&req, &owner, &repo) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let git_ref = query.r#ref.clone().unwrap_or_else(|| "HEAD".to_string());
    let sub_path = query.path.clone().unwrap_or_default();
    let spec = if sub_path.is_empty() {
        format!("{git_ref}:")
    } else {
        let trimmed = sub_path.trim_end_matches('/');
        format!("{git_ref}:{trimmed}/")
    };

    let output = Command::new("git")
        .args(["-C", &repo_dir, "ls-tree", "--long", &spec])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let mut entries = Vec::new();
            for line in stdout.lines() {
                let mut head_and_name = line.splitn(2, '\t');
                let head = head_and_name.next().unwrap_or("");
                let name = head_and_name.next().unwrap_or("");
                let cols: Vec<&str> = head.split_whitespace().collect();
                if cols.len() >= 4 {
                    let size = if cols[3] == "-" { None } else { cols[3].parse::<u64>().ok() };
                    entries.push(serde_json::json!({
                        "mode": cols[0],
                        "type": cols[1],
                        "sha": cols[2],
                        "size": size,
                        "name": name,
                    }));
                }
            }
            HttpResponse::Ok().json(serde_json::json!({
                "ref": git_ref,
                "path": sub_path,
                "entries": entries,
            }))
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            eprintln!("[repos] git ls-tree failed: {stderr}");
            HttpResponse::NotFound()
                .json(serde_json::json!({"error": "tree not found"}))
        }
        Err(e) => {
            eprintln!("[repos] Failed to run git: {e}");
            HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Failed to run git command"}))
        }
    }
}

/// `GET /repositories/{owner}/{repo}/blob`
///
/// Returns the contents of a single file at a given ref, base64-encoded.
pub async fn get_blob(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    query: web::Query<BlobQuery>,
) -> HttpResponse {
    let (owner, repo) = path.into_inner();
    let repo_dir = match require_owner(&req, &owner, &repo) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let git_ref = query.r#ref.clone().unwrap_or_else(|| "HEAD".to_string());
    let spec = format!("{}:{}", git_ref, query.path);

    let output = Command::new("git")
        .args(["-C", &repo_dir, "show", &spec])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let bytes = out.stdout;
            let scan_len = bytes.len().min(8000);
            let is_binary = bytes[..scan_len].contains(&0u8);
            let size = bytes.len();
            let encoded = BASE64.encode(&bytes);
            HttpResponse::Ok().json(serde_json::json!({
                "ref": git_ref,
                "path": query.path,
                "content": encoded,
                "is_binary": is_binary,
                "size": size,
            }))
        }
        Ok(_) => HttpResponse::NotFound()
            .json(serde_json::json!({"error": "file not found"})),
        Err(e) => {
            eprintln!("[repos] Failed to run git: {e}");
            HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Failed to run git command"}))
        }
    }
}

/// `GET /repositories/{owner}/{repo}/commits/{sha}/diff`
///
/// Returns commit metadata, file stats, and the unified diff text.
pub async fn get_diff(
    req: HttpRequest,
    path: web::Path<(String, String, String)>,
) -> HttpResponse {
    let (owner, repo, sha) = path.into_inner();
    let repo_dir = match require_owner(&req, &owner, &repo) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    if !sha.chars().all(|c| c.is_ascii_hexdigit()) || sha.is_empty() {
        return HttpResponse::BadRequest()
            .json(serde_json::json!({"error": "invalid sha"}));
    }

    let output = Command::new("git")
        .args([
            "-C", &repo_dir, "show",
            "--format=%H%x00%an%x00%ae%x00%at%x00%s%x00",
            "--stat", "-p", &sha,
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
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
                if trimmed.contains("file changed") || trimmed.contains("files changed") {
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
            }

            HttpResponse::Ok().json(serde_json::json!({
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
            }))
        }
        Ok(_) => HttpResponse::NotFound()
            .json(serde_json::json!({"error": "commit not found"})),
        Err(e) => {
            eprintln!("[repos] Failed to run git: {e}");
            HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "Failed to run git command"}))
        }
    }
}

// ── Write endpoints ──────────────────────────────────────────────────────────

/// Internal result of running the plumbing sequence: the new commit SHA.
fn run_write_sequence(
    repo_dir: &str,
    branch: &str,
    message: &str,
    author_name: &str,
    author_email: &str,
    update: WriteUpdate,
) -> Result<String, HttpResponse> {
    let lock = write_lock(repo_dir);
    let _guard = lock.lock().unwrap();

    // Read the branch's tree into the index.
    let read_tree = Command::new("git")
        .args(["-C", repo_dir, "read-tree", branch])
        .output();
    if let Err(e) = read_tree {
        eprintln!("[repos] read-tree failed: {e}");
        return Err(HttpResponse::InternalServerError()
            .json(serde_json::json!({"error": "read-tree failed"})));
    }
    let read_tree = read_tree.unwrap();
    if !read_tree.status.success() {
        eprintln!("[repos] read-tree: {}", String::from_utf8_lossy(&read_tree.stderr));
        return Err(HttpResponse::InternalServerError()
            .json(serde_json::json!({"error": "read-tree failed"})));
    }

    // Either add a blob or remove an entry.
    match update {
        WriteUpdate::Add { path, content } => {
            let mut hash = Command::new("git")
                .args(["-C", repo_dir, "hash-object", "-w", "--stdin"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| {
                    eprintln!("[repos] hash-object spawn failed: {e}");
                    HttpResponse::InternalServerError()
                        .json(serde_json::json!({"error": "hash-object failed"}))
                })?;
            {
                use std::io::Write;
                let stdin = hash.stdin.as_mut().unwrap();
                stdin.write_all(&content).map_err(|e| {
                    eprintln!("[repos] hash-object write failed: {e}");
                    HttpResponse::InternalServerError()
                        .json(serde_json::json!({"error": "hash-object write failed"}))
                })?;
            }
            let hash_out = hash.wait_with_output().map_err(|e| {
                eprintln!("[repos] hash-object wait failed: {e}");
                HttpResponse::InternalServerError()
                    .json(serde_json::json!({"error": "hash-object wait failed"}))
            })?;
            if !hash_out.status.success() {
                return Err(HttpResponse::InternalServerError()
                    .json(serde_json::json!({"error": "hash-object failed"})));
            }
            let blob_sha = String::from_utf8_lossy(&hash_out.stdout).trim().to_string();
            let cacheinfo = format!("100644,{blob_sha},{path}");
            let upd = Command::new("git")
                .args(["-C", repo_dir, "update-index", "--add", "--cacheinfo", &cacheinfo])
                .output();
            match upd {
                Ok(o) if o.status.success() => {}
                Ok(o) => {
                    eprintln!("[repos] update-index: {}", String::from_utf8_lossy(&o.stderr));
                    return Err(HttpResponse::InternalServerError()
                        .json(serde_json::json!({"error": "update-index failed"})));
                }
                Err(e) => {
                    eprintln!("[repos] update-index error: {e}");
                    return Err(HttpResponse::InternalServerError()
                        .json(serde_json::json!({"error": "update-index failed"})));
                }
            }
        }
        WriteUpdate::Remove { path } => {
            // `update-index --force-remove` insists on a non-bare repo. Point
            // GIT_WORK_TREE at a throwaway temp directory so the operation
            // succeeds without ever touching the (bare) repo's directory.
            let work_tree = std::env::temp_dir()
                .join(format!("git-agent-wt-{}", Uuid::new_v4().simple()));
            let _ = std::fs::create_dir_all(&work_tree);
            let upd = Command::new("git")
                .args([
                    "-C", repo_dir,
                    "-c", "core.bare=false",
                    "update-index", "--force-remove", &path,
                ])
                .env("GIT_WORK_TREE", &work_tree)
                .output();
            let _ = std::fs::remove_dir_all(&work_tree);
            match upd {
                Ok(o) if o.status.success() => {}
                Ok(o) => {
                    eprintln!("[repos] update-index --remove: {}", String::from_utf8_lossy(&o.stderr));
                    return Err(HttpResponse::InternalServerError()
                        .json(serde_json::json!({"error": "update-index failed"})));
                }
                Err(e) => {
                    eprintln!("[repos] update-index --remove error: {e}");
                    return Err(HttpResponse::InternalServerError()
                        .json(serde_json::json!({"error": "update-index failed"})));
                }
            }
        }
    }

    // Write the index out to a tree object.
    let write_tree = Command::new("git")
        .args(["-C", repo_dir, "write-tree"])
        .output()
        .map_err(|e| {
            eprintln!("[repos] write-tree error: {e}");
            HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "write-tree failed"}))
        })?;
    if !write_tree.status.success() {
        eprintln!("[repos] write-tree: {}", String::from_utf8_lossy(&write_tree.stderr));
        return Err(HttpResponse::InternalServerError()
            .json(serde_json::json!({"error": "write-tree failed"})));
    }
    let tree_sha = String::from_utf8_lossy(&write_tree.stdout).trim().to_string();

    // Resolve the parent commit for the branch.
    let parent_out = Command::new("git")
        .args(["-C", repo_dir, "rev-parse", branch])
        .output()
        .map_err(|e| {
            eprintln!("[repos] rev-parse error: {e}");
            HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "rev-parse failed"}))
        })?;
    if !parent_out.status.success() {
        return Err(HttpResponse::InternalServerError()
            .json(serde_json::json!({"error": "branch not found"})));
    }
    let parent_sha = String::from_utf8_lossy(&parent_out.stdout).trim().to_string();

    // Create the commit object with author/committer env vars.
    let commit_out = Command::new("git")
        .args(["-C", repo_dir, "commit-tree", &tree_sha, "-p", &parent_sha, "-m", message])
        .env("GIT_AUTHOR_NAME", author_name)
        .env("GIT_AUTHOR_EMAIL", author_email)
        .env("GIT_COMMITTER_NAME", author_name)
        .env("GIT_COMMITTER_EMAIL", author_email)
        .output()
        .map_err(|e| {
            eprintln!("[repos] commit-tree error: {e}");
            HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "commit-tree failed"}))
        })?;
    if !commit_out.status.success() {
        eprintln!("[repos] commit-tree: {}", String::from_utf8_lossy(&commit_out.stderr));
        return Err(HttpResponse::InternalServerError()
            .json(serde_json::json!({"error": "commit-tree failed"})));
    }
    let new_sha = String::from_utf8_lossy(&commit_out.stdout).trim().to_string();

    // Advance the branch ref.
    let upd_ref = Command::new("git")
        .args(["-C", repo_dir, "update-ref", &format!("refs/heads/{branch}"), &new_sha])
        .output()
        .map_err(|e| {
            eprintln!("[repos] update-ref error: {e}");
            HttpResponse::InternalServerError()
                .json(serde_json::json!({"error": "update-ref failed"}))
        })?;
    if !upd_ref.status.success() {
        return Err(HttpResponse::InternalServerError()
            .json(serde_json::json!({"error": "update-ref failed"})));
    }

    Ok(new_sha)
}

enum WriteUpdate {
    Add { path: String, content: Vec<u8> },
    Remove { path: String },
}

/// Returns true if `path` exists in the given branch's tree.
fn path_exists(repo_dir: &str, branch: &str, path: &str) -> bool {
    let spec = format!("{branch}:{path}");
    Command::new("git")
        .args(["-C", repo_dir, "cat-file", "-e", &spec])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `POST /repositories/{owner}/{repo}/blob`
///
/// Creates a new file at the given path on the target branch and commits it.
/// Returns 409 if the file already exists.
pub async fn create_blob(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    body: web::Json<WriteFileBody>,
) -> HttpResponse {
    let (owner, repo) = path.into_inner();
    let repo_dir = match require_owner(&req, &owner, &repo) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    if body.path.trim().is_empty() {
        return HttpResponse::BadRequest()
            .json(serde_json::json!({"error": "path is required"}));
    }

    if path_exists(&repo_dir, &body.branch, &body.path) {
        return HttpResponse::Conflict()
            .json(serde_json::json!({"error": "file already exists"}));
    }

    let content = match BASE64.decode(body.content.as_bytes()) {
        Ok(b) => b,
        Err(_) => return HttpResponse::BadRequest()
            .json(serde_json::json!({"error": "content is not valid base64"})),
    };

    match run_write_sequence(
        &repo_dir,
        &body.branch,
        &body.message,
        &body.author_name,
        &body.author_email,
        WriteUpdate::Add { path: body.path.clone(), content },
    ) {
        Ok(sha) => HttpResponse::Created().json(serde_json::json!({
            "sha": sha,
            "path": body.path,
            "branch": body.branch,
        })),
        Err(resp) => resp,
    }
}

/// `PUT /repositories/{owner}/{repo}/blob`
///
/// Overwrites an existing file at the given path. Returns 404 if the file
/// does not exist on the target branch.
pub async fn update_blob(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    body: web::Json<WriteFileBody>,
) -> HttpResponse {
    let (owner, repo) = path.into_inner();
    let repo_dir = match require_owner(&req, &owner, &repo) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    if !path_exists(&repo_dir, &body.branch, &body.path) {
        return HttpResponse::NotFound()
            .json(serde_json::json!({"error": "file not found"}));
    }

    let content = match BASE64.decode(body.content.as_bytes()) {
        Ok(b) => b,
        Err(_) => return HttpResponse::BadRequest()
            .json(serde_json::json!({"error": "content is not valid base64"})),
    };

    match run_write_sequence(
        &repo_dir,
        &body.branch,
        &body.message,
        &body.author_name,
        &body.author_email,
        WriteUpdate::Add { path: body.path.clone(), content },
    ) {
        Ok(sha) => HttpResponse::Ok().json(serde_json::json!({
            "sha": sha,
            "path": body.path,
            "branch": body.branch,
        })),
        Err(resp) => resp,
    }
}

/// `DELETE /repositories/{owner}/{repo}/blob`
///
/// Removes a file from the target branch and commits the deletion.
pub async fn delete_blob(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    body: web::Json<DeleteFileBody>,
) -> HttpResponse {
    let (owner, repo) = path.into_inner();
    let repo_dir = match require_owner(&req, &owner, &repo) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    if !path_exists(&repo_dir, &body.branch, &body.path) {
        return HttpResponse::NotFound()
            .json(serde_json::json!({"error": "file not found"}));
    }

    match run_write_sequence(
        &repo_dir,
        &body.branch,
        &body.message,
        &body.author_name,
        &body.author_email,
        WriteUpdate::Remove { path: body.path.clone() },
    ) {
        Ok(sha) => HttpResponse::Ok().json(serde_json::json!({
            "sha": sha,
            "path": body.path,
            "branch": body.branch,
        })),
        Err(resp) => resp,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test, web, App};

    /// Build a minimal test app with just the repository route.
    fn test_app() -> actix_web::App<
        impl actix_web::dev::ServiceFactory<
            actix_web::dev::ServiceRequest,
            Config = (),
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
            InitError = (),
        >,
    > {
        App::new().route("/repositories", web::post().to(create_repository))
    }

    #[actix_web::test]
    async fn test_create_repo_requires_auth() {
        let app = test::init_service(test_app()).await;
        let req = test::TestRequest::post()
            .uri("/repositories")
            .set_json(serde_json::json!({"name": "test-repo", "user_id": "123"}))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    #[actix_web::test]
    async fn test_create_repo_rejects_invalid_jwt() {
        unsafe { std::env::set_var("JWT_SECRET", "test-secret-32-chars-long-enough!!"); }
        let app = test::init_service(test_app()).await;
        let req = test::TestRequest::post()
            .uri("/repositories")
            .insert_header(("Authorization", "Bearer bad.token.here"))
            .set_json(serde_json::json!({"name": "test-repo", "user_id": "123"}))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    #[actix_web::test]
    async fn test_create_repo_rejects_empty_name() {
        use jsonwebtoken::{encode, EncodingKey, Header};
        use crate::auth::Claims;

        unsafe { std::env::set_var("JWT_SECRET", "test-secret-32-chars-long-enough!!"); }
        let claims = Claims {
            sub: "user123".to_string(),
            email: "u@example.com".to_string(),
            username: "user123".to_string(),
            iat: None,
            exp: Some(9_999_999_999),
        };
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(b"test-secret-32-chars-long-enough!!"),
        )
        .unwrap();

        let app = test::init_service(test_app()).await;
        let req = test::TestRequest::post()
            .uri("/repositories")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(serde_json::json!({"name": "", "user_id": "user123"}))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);
    }

    #[actix_web::test]
    async fn test_create_repo_rejects_mismatched_user_id() {
        use jsonwebtoken::{encode, EncodingKey, Header};
        use crate::auth::Claims;

        unsafe { std::env::set_var("JWT_SECRET", "test-secret-32-chars-long-enough!!"); }
        let claims = Claims {
            sub: "user123".to_string(),
            email: "u@example.com".to_string(),
            username: "user123".to_string(),
            iat: None,
            exp: Some(9_999_999_999),
        };
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(b"test-secret-32-chars-long-enough!!"),
        )
        .unwrap();

        let app = test::init_service(test_app()).await;
        let req = test::TestRequest::post()
            .uri("/repositories")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(serde_json::json!({"name": "myrepo", "user_id": "attacker"}))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 403);
    }

    // ── Helpers for the new endpoint tests ──────────────────────────────────

    fn make_token(sub: &str) -> String {
        use jsonwebtoken::{encode, EncodingKey, Header};
        use crate::auth::Claims;
        unsafe { std::env::set_var("JWT_SECRET", "test-secret-32-chars-long-enough!!"); }
        let claims = Claims {
            sub: sub.to_string(),
            email: "u@example.com".to_string(),
            username: sub.to_string(),
            iat: None,
            exp: Some(9_999_999_999),
        };
        encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(b"test-secret-32-chars-long-enough!!"),
        )
        .unwrap()
    }

    /// Create a unique, isolated bare repository on disk and seed it with a
    /// single commit on `main` containing a `README.md`. Returns the test
    /// (owner, repo) name pair.
    fn seed_repo() -> (String, String) {
        // All tests share one REPOS_DIR; uniqueness comes from the owner name.
        let tmp_root = std::env::temp_dir().join("git-agent-tests-shared");
        std::fs::create_dir_all(&tmp_root).unwrap();
        unsafe { std::env::set_var("REPOS_DIR", &tmp_root); }

        let unique = Uuid::new_v4().simple().to_string();
        let owner = format!("user{unique}");
        let repo_name = format!("repo{unique}");
        let repo_dir = tmp_root.join(&owner).join(format!("{repo_name}.git"));
        std::fs::create_dir_all(&repo_dir).unwrap();

        let repo_dir_str = repo_dir.to_string_lossy().to_string();
        let _ = Command::new("git").args(["init", "--bare", &repo_dir_str]).output().unwrap();

        // hash a README blob
        let mut hash = Command::new("git")
            .args(["-C", &repo_dir_str, "hash-object", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::Write;
            hash.stdin.as_mut().unwrap().write_all(b"# Hello\n").unwrap();
        }
        let blob = hash.wait_with_output().unwrap();
        let blob_sha = String::from_utf8_lossy(&blob.stdout).trim().to_string();

        // build a tree from the blob
        let mktree_input = format!("100644 blob {blob_sha}\tREADME.md\n");
        let mut mktree = Command::new("git")
            .args(["-C", &repo_dir_str, "mktree"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::Write;
            mktree.stdin.as_mut().unwrap().write_all(mktree_input.as_bytes()).unwrap();
        }
        let tree_out = mktree.wait_with_output().unwrap();
        let tree_sha = String::from_utf8_lossy(&tree_out.stdout).trim().to_string();

        // commit
        let commit = Command::new("git")
            .args(["-C", &repo_dir_str, "commit-tree", &tree_sha, "-m", "init"])
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        let commit_sha = String::from_utf8_lossy(&commit.stdout).trim().to_string();

        let _ = Command::new("git")
            .args(["-C", &repo_dir_str, "update-ref", "refs/heads/main", &commit_sha])
            .output()
            .unwrap();

        (owner, repo_name)
    }

    fn full_app() -> actix_web::App<
        impl actix_web::dev::ServiceFactory<
            actix_web::dev::ServiceRequest,
            Config = (),
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
            InitError = (),
        >,
    > {
        App::new()
            .route("/repositories/{owner}/{repo}/branches", web::get().to(list_branches))
            .route("/repositories/{owner}/{repo}/commits", web::get().to(list_commits))
            .route("/repositories/{owner}/{repo}/commits/{sha}/diff", web::get().to(get_diff))
            .route("/repositories/{owner}/{repo}/tree", web::get().to(get_tree))
            .route("/repositories/{owner}/{repo}/blob", web::get().to(get_blob))
            .route("/repositories/{owner}/{repo}/blob", web::post().to(create_blob))
            .route("/repositories/{owner}/{repo}/blob", web::put().to(update_blob))
            .route("/repositories/{owner}/{repo}/blob", web::delete().to(delete_blob))
    }

    // ── 401 / unauth tests ──────────────────────────────────────────────────

    #[actix_web::test]
    async fn test_list_branches_requires_auth() {
        let app = test::init_service(full_app()).await;
        let req = test::TestRequest::get().uri("/repositories/u/r/branches").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    #[actix_web::test]
    async fn test_list_commits_requires_auth() {
        let app = test::init_service(full_app()).await;
        let req = test::TestRequest::get().uri("/repositories/u/r/commits").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    #[actix_web::test]
    async fn test_get_diff_requires_auth() {
        let app = test::init_service(full_app()).await;
        let req = test::TestRequest::get().uri("/repositories/u/r/commits/abc/diff").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    #[actix_web::test]
    async fn test_get_tree_requires_auth() {
        let app = test::init_service(full_app()).await;
        let req = test::TestRequest::get().uri("/repositories/u/r/tree").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    #[actix_web::test]
    async fn test_get_blob_requires_auth() {
        let app = test::init_service(full_app()).await;
        let req = test::TestRequest::get().uri("/repositories/u/r/blob?path=README.md").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    #[actix_web::test]
    async fn test_create_blob_requires_auth() {
        let app = test::init_service(full_app()).await;
        let req = test::TestRequest::post()
            .uri("/repositories/u/r/blob")
            .set_json(serde_json::json!({
                "path": "f", "content": "", "message": "m", "branch": "main",
                "author_name": "a", "author_email": "a@a"
            }))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    #[actix_web::test]
    async fn test_update_blob_requires_auth() {
        let app = test::init_service(full_app()).await;
        let req = test::TestRequest::put()
            .uri("/repositories/u/r/blob")
            .set_json(serde_json::json!({
                "path": "f", "content": "", "message": "m", "branch": "main",
                "author_name": "a", "author_email": "a@a"
            }))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    #[actix_web::test]
    async fn test_delete_blob_requires_auth() {
        let app = test::init_service(full_app()).await;
        let req = test::TestRequest::delete()
            .uri("/repositories/u/r/blob")
            .set_json(serde_json::json!({
                "path": "f", "message": "m", "branch": "main",
                "author_name": "a", "author_email": "a@a"
            }))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    // ── Success-shape tests against a seeded bare repo ──────────────────────

    #[actix_web::test]
    async fn test_list_branches_returns_main() {
        let (owner, repo) = seed_repo();
        let token = make_token(&owner);
        let app = test::init_service(full_app()).await;

        let req = test::TestRequest::get()
            .uri(&format!("/repositories/{owner}/{repo}/branches"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = test::read_body_json(resp).await;
        let branches = body["branches"].as_array().unwrap();
        assert!(branches.iter().any(|b| b == "main"));
    }

    #[actix_web::test]
    async fn test_list_commits_returns_seed_commit() {
        let (owner, repo) = seed_repo();
        let token = make_token(&owner);
        let app = test::init_service(full_app()).await;

        let req = test::TestRequest::get()
            .uri(&format!("/repositories/{owner}/{repo}/commits?branch=main"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = test::read_body_json(resp).await;
        let commits = body["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0]["message"], "init");
    }

    #[actix_web::test]
    async fn test_get_tree_returns_readme() {
        let (owner, repo) = seed_repo();
        let token = make_token(&owner);
        let app = test::init_service(full_app()).await;

        let req = test::TestRequest::get()
            .uri(&format!("/repositories/{owner}/{repo}/tree?ref=main"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = test::read_body_json(resp).await;
        let entries = body["entries"].as_array().unwrap();
        assert!(entries.iter().any(|e| e["name"] == "README.md"));
    }

    #[actix_web::test]
    async fn test_get_blob_returns_readme_content() {
        let (owner, repo) = seed_repo();
        let token = make_token(&owner);
        let app = test::init_service(full_app()).await;

        let req = test::TestRequest::get()
            .uri(&format!("/repositories/{owner}/{repo}/blob?ref=main&path=README.md"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["is_binary"], false);
        let decoded = BASE64.decode(body["content"].as_str().unwrap()).unwrap();
        assert_eq!(decoded, b"# Hello\n");
    }

    #[actix_web::test]
    async fn test_create_update_delete_blob_roundtrip() {
        let (owner, repo) = seed_repo();
        let token = make_token(&owner);
        let app = test::init_service(full_app()).await;

        // Create
        let body = serde_json::json!({
            "path": "src/lib.rs",
            "content": BASE64.encode(b"hello"),
            "message": "feat: add lib",
            "branch": "main",
            "author_name": "Test",
            "author_email": "t@example.com"
        });
        let req = test::TestRequest::post()
            .uri(&format!("/repositories/{owner}/{repo}/blob"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(&body)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 201);

        // Conflict on second create
        let req = test::TestRequest::post()
            .uri(&format!("/repositories/{owner}/{repo}/blob"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(&body)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 409);

        // Update
        let upd = serde_json::json!({
            "path": "src/lib.rs",
            "content": BASE64.encode(b"world"),
            "message": "chore: update lib",
            "branch": "main",
            "author_name": "Test",
            "author_email": "t@example.com"
        });
        let req = test::TestRequest::put()
            .uri(&format!("/repositories/{owner}/{repo}/blob"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(&upd)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);

        // Delete
        let del = serde_json::json!({
            "path": "src/lib.rs",
            "message": "chore: rm lib",
            "branch": "main",
            "author_name": "Test",
            "author_email": "t@example.com"
        });
        let req = test::TestRequest::delete()
            .uri(&format!("/repositories/{owner}/{repo}/blob"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(&del)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);

        // 404 on delete of missing file
        let req = test::TestRequest::delete()
            .uri(&format!("/repositories/{owner}/{repo}/blob"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(&del)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }

    #[actix_web::test]
    async fn test_get_diff_returns_seed_commit_diff() {
        let (owner, repo) = seed_repo();
        let token = make_token(&owner);
        let app = test::init_service(full_app()).await;

        // Resolve the seed commit SHA via the commits endpoint
        let req = test::TestRequest::get()
            .uri(&format!("/repositories/{owner}/{repo}/commits?branch=main"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        let resp = test::call_service(&app, req).await;
        let body: serde_json::Value = test::read_body_json(resp).await;
        let sha = body["commits"][0]["sha"].as_str().unwrap().to_string();

        let req = test::TestRequest::get()
            .uri(&format!("/repositories/{owner}/{repo}/commits/{sha}/diff"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["sha"], sha);
        assert!(body["diff"].as_str().unwrap().contains("README.md"));
    }
}
