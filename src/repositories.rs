use actix_web::{web, HttpRequest, HttpResponse};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{env, fs, process::Command};
use uuid::Uuid;

use crate::auth::require_auth;

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
        std::env::set_var("JWT_SECRET", "test-secret");
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

        std::env::set_var("JWT_SECRET", "test-secret-32-chars-long-enough!!");
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

        std::env::set_var("JWT_SECRET", "test-secret-32-chars-long-enough!!");
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
}
