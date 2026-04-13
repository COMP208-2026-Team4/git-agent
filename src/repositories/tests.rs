//! Integration-style tests for the repositories module. The tests stand up a
//! minimal `actix_web::App`, seed bare repos on disk, and exercise the full
//! HTTP path through to the git CLI.

use std::process::Command;

use actix_web::{test, web, App};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use uuid::Uuid;

use crate::auth::Claims;

use super::handlers::{
    create_blob, create_repository, delete_blob, get_blob, get_diff, get_tree, list_branches,
    list_commits, profile_repos, update_blob,
};

// ── Token / fixture helpers ─────────────────────────────────────────────────

fn make_token_with_username(sub: &str, username: &str) -> String {
    use jsonwebtoken::{encode, EncodingKey, Header};
    unsafe { std::env::set_var("JWT_SECRET", "test-secret-32-chars-long-enough!!"); }
    let claims = Claims {
        sub: sub.to_string(),
        email: "u@example.com".to_string(),
        username: username.to_string(),
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

fn make_token(sub: &str) -> String {
    make_token_with_username(sub, sub)
}

fn shared_repos_root() -> std::path::PathBuf {
    let tmp_root = std::env::temp_dir().join("git-agent-tests-shared");
    std::fs::create_dir_all(&tmp_root).unwrap();
    unsafe { std::env::set_var("REPOS_DIR", &tmp_root); }
    tmp_root
}

/// Create a unique, isolated bare repository on disk and seed it with a
/// single commit on `main` containing a `README.md`.
fn seed_repo() -> (String, String) {
    let tmp_root = shared_repos_root();

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

/// Create an empty bare repository with no commits.
fn seed_empty_repo() -> (String, String) {
    let tmp_root = shared_repos_root();

    let unique = Uuid::new_v4().simple().to_string();
    let owner = format!("user{unique}");
    let repo_name = format!("repo{unique}");
    let repo_dir = tmp_root.join(&owner).join(format!("{repo_name}.git"));
    std::fs::create_dir_all(repo_dir.parent().unwrap()).unwrap();

    let repo_dir_str = repo_dir.to_string_lossy().to_string();
    let _ = Command::new("git")
        .args(["init", "--bare", &repo_dir_str])
        .output()
        .unwrap();

    (owner, repo_name)
}

fn create_repo_app() -> actix_web::App<
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

// ── create_repository ───────────────────────────────────────────────────────

#[actix_web::test]
async fn test_create_repo_requires_auth() {
    let app = test::init_service(create_repo_app()).await;
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
    let app = test::init_service(create_repo_app()).await;
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
    let token = make_token("user123");
    let app = test::init_service(create_repo_app()).await;
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
    let token = make_token("user123");
    let app = test::init_service(create_repo_app()).await;
    let req = test::TestRequest::post()
        .uri("/repositories")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(serde_json::json!({"name": "myrepo", "user_id": "attacker"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 403);
}

#[actix_web::test]
async fn test_create_blob_initializes_missing_branch() {
    let (owner, repo) = seed_empty_repo();
    let token = make_token(&owner);
    let app = test::init_service(full_app()).await;

    let body = serde_json::json!({
        "path": "README.md",
        "content": BASE64.encode(b"hello"),
        "message": "feat: initial commit",
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

// ── 401 / unauth tests ──────────────────────────────────────────────────────

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

// ── Success-shape tests against a seeded bare repo ──────────────────────────

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
async fn test_owner_routes_allow_username_alias() {
    let (owner, repo) = seed_repo();
    let token = make_token_with_username(&owner, "display-user");
    let app = test::init_service(full_app()).await;

    let req = test::TestRequest::get()
        .uri(&format!("/repositories/display-user/{repo}/branches"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
}

#[actix_web::test]
async fn test_owner_routes_allow_username_alias_case_insensitive() {
    let (owner, repo) = seed_repo();
    let token = make_token_with_username(&owner, "Display-User");
    let app = test::init_service(full_app()).await;

    // Path uses a different casing than the claims username - must still pass.
    let req = test::TestRequest::get()
        .uri(&format!("/repositories/display-user/{repo}/branches"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
}

#[actix_web::test]
async fn test_owner_routes_reject_unrelated_owner() {
    let (owner, repo) = seed_repo();
    let token = make_token_with_username(&owner, "display-user");
    let app = test::init_service(full_app()).await;

    let req = test::TestRequest::get()
        .uri(&format!("/repositories/another-user/{repo}/branches"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 403);
}

#[actix_web::test]
async fn test_canonical_storage_path_uses_sub_not_username() {
    // Even when the URL owner is the username alias, the on-disk repo lives
    // under `claims.sub`. We seed under `sub`, send the request via the
    // username alias, and assert success - proving the canonical path is
    // resolved from `sub` regardless of the path label used to reach the route.
    let (owner_sub, repo) = seed_repo();
    let token = make_token_with_username(&owner_sub, "human-name");
    let app = test::init_service(full_app()).await;

    let req = test::TestRequest::get()
        .uri(&format!("/repositories/human-name/{repo}/commits?branch=main"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["commits"].as_array().unwrap().len(), 1);
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
async fn test_list_branches_includes_head() {
    // Cold-load fix: list_branches must surface the symbolic HEAD so the
    // frontend can default to the real branch instead of the hard-coded
    // "main" that 500'd repos with a different default.
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
    assert_eq!(body["head"], "main");
}

#[actix_web::test]
async fn test_list_commits_unknown_branch_is_empty_not_500() {
    // Direct-load fix: hitting /commits?branch=main on a fresh repo (or
    // any repo whose default branch isn't `main`) must NOT 500 with
    // "git log failed". The handler returns an empty list so the
    // frontend can render and re-fetch with the resolved HEAD.
    let (owner, repo) = seed_empty_repo();
    let token = make_token(&owner);
    let app = test::init_service(full_app()).await;

    let req = test::TestRequest::get()
        .uri(&format!("/repositories/{owner}/{repo}/commits?branch=main"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["commits"].as_array().unwrap().len(), 0);
}

#[actix_web::test]
async fn test_get_tree_unknown_branch_is_empty_not_404() {
    // Direct-load fix: hitting /tree?ref=main cold on a repo without that
    // branch must return an empty tree (200), not a 404 that breaks the
    // RepoPage's first render.
    let (owner, repo) = seed_empty_repo();
    let token = make_token(&owner);
    let app = test::init_service(full_app()).await;

    let req = test::TestRequest::get()
        .uri(&format!("/repositories/{owner}/{repo}/tree?ref=main"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["entries"].as_array().unwrap().len(), 0);
}

#[actix_web::test]
async fn test_username_owner_route_for_commits_and_tree() {
    // Regression: commits/tree must accept the username owner segment, not
    // just the snowflake sub. Direct loading e.g. /adamfoster_3888/repo/commits
    // resolves to the same on-disk repo as the canonical sub.
    let (owner_sub, repo) = seed_repo();
    let token = make_token_with_username(&owner_sub, "adamfoster_3888");
    let app = test::init_service(full_app()).await;

    for url in [
        format!("/repositories/adamfoster_3888/{repo}/commits?branch=main"),
        format!("/repositories/adamfoster_3888/{repo}/tree?ref=main"),
    ] {
        let req = test::TestRequest::get()
            .uri(&url)
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200, "expected 200 for {url}");
    }
}

#[actix_web::test]
async fn test_profile_repos_returns_public_repos_by_username() {
    // Regression: profile_repos must resolve a username to the owner's
    // snowflake-ID directory and return public repos for authenticated callers.
    let tmp_root = shared_repos_root();
    let unique = Uuid::new_v4().simple().to_string();
    let owner_sub = format!("9000{unique}");
    let username = format!("profileuser{unique}");
    let repo_name = format!("pub{unique}");

    let repo_dir = tmp_root.join(&owner_sub).join(format!("{repo_name}.git"));
    std::fs::create_dir_all(&repo_dir).unwrap();
    Command::new("git")
        .args(["init", "--bare", &repo_dir.to_string_lossy().to_string()])
        .output()
        .unwrap();
    let meta_path = tmp_root.join(&owner_sub).join(format!("{repo_name}.meta.json"));
    std::fs::write(
        &meta_path,
        serde_json::json!({
            "visibility": "public",
            "description": "",
            "stars": [],
            "collaborators": [],
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z"
        })
        .to_string(),
    )
    .unwrap();

    let token = make_token_with_username(&owner_sub, &username);
    let app = test::init_service(
        App::new().route("/repositories/profile/{owner}", web::get().to(profile_repos)),
    )
    .await;

    let req = test::TestRequest::get()
        .uri(&format!("/repositories/profile/{username}"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    let repos = body.as_array().unwrap();
    assert!(
        repos.iter().any(|r| r["name"].as_str() == Some(&repo_name)),
        "expected public repo '{repo_name}' in profile listing, got: {body}"
    );
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
