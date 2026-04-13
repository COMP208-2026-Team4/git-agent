mod auth;
mod repositories;

use actix_cors::Cors;
use actix_web::{get, web, App, HttpResponse, HttpServer, Responder};
use dotenvy::dotenv;
use std::env;

#[get("/health")]
async fn health() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "status": "ok",
        "timestamp": chrono::Utc::now().to_rfc3339()
    }))
}

fn create_cors() -> Cors {
    let allowed_origins = env::var("CORS_ALLOWED_ORIGINS").unwrap_or_else(|_| "*".to_string());
    let mut cors = Cors::default()
        .allow_any_method()
        .allow_any_header();

    if allowed_origins == "*" {
        cors = cors.allow_any_origin();
    } else {
        for origin in allowed_origins.split(',') {
            cors = cors.allowed_origin(origin.trim());
        }
    }
    cors.supports_credentials()
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    dotenv().ok();

    let port: u16 = env::var("PORT")
        .unwrap_or_else(|_| "6025".to_string())
        .parse()
        .expect("PORT must be a valid port number");

    let bind_addr = env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0".to_string());

    println!("git-agent listening on http://{bind_addr}:{port}");

    HttpServer::new(|| {
        let cors = create_cors();

        App::new()
            .wrap(cors)
            .service(health)
            // ── Search ──────────────────────────────────────────
            .route("/search", web::get().to(repositories::search))
            .route("/git/search", web::get().to(repositories::search))
            // ── Repo CRUD ───────────────────────────────────────
            .route(
                "/repositories",
                web::get().to(repositories::list_repositories),
            )
            .route(
                "/repositories",
                web::post().to(repositories::create_repository),
            )
            .route(
                "/repositories/{owner}/{repo}",
                web::delete().to(repositories::delete_repository),
            )
            // ── Profile repos (public listing) ──────────────────
            .route(
                "/repositories/profile/{owner}",
                web::get().to(repositories::profile_repos),
            )
            // ── Repo metadata & settings ────────────────────────
            .route(
                "/repositories/{owner}/{repo}/meta",
                web::get().to(repositories::get_repo_meta),
            )
            .route(
                "/repositories/{owner}/{repo}/settings",
                web::put().to(repositories::update_settings),
            )
            // ── Stars ───────────────────────────────────────────
            .route(
                "/repositories/{owner}/{repo}/star",
                web::post().to(repositories::star_repo),
            )
            .route(
                "/repositories/{owner}/{repo}/star",
                web::delete().to(repositories::unstar_repo),
            )
            // ── Collaborators ───────────────────────────────────
            .route(
                "/repositories/{owner}/{repo}/collaborators",
                web::post().to(repositories::add_collaborator),
            )
            .route(
                "/repositories/{owner}/{repo}/collaborators/{user_id}",
                web::delete().to(repositories::remove_collaborator),
            )
            // ── Git read endpoints ──────────────────────────────
            .route(
                "/repositories/{owner}/{repo}/branches",
                web::get().to(repositories::list_branches),
            )
            .route(
                "/repositories/{owner}/{repo}/commits",
                web::get().to(repositories::list_commits),
            )
            .route(
                "/repositories/{owner}/{repo}/commits/{sha}/diff",
                web::get().to(repositories::get_diff),
            )
            .route(
                "/repositories/{owner}/{repo}/commits/{sha}/preview",
                web::get().to(repositories::commit_preview),
            )
            .route(
                "/repositories/{owner}/{repo}/tree",
                web::get().to(repositories::get_tree),
            )
            // ── Git blob CRUD ───────────────────────────────────
            .route(
                "/repositories/{owner}/{repo}/blob",
                web::get().to(repositories::get_blob),
            )
            .route(
                "/repositories/{owner}/{repo}/blob",
                web::post().to(repositories::create_blob),
            )
            .route(
                "/repositories/{owner}/{repo}/blob",
                web::put().to(repositories::update_blob),
            )
            .route(
                "/repositories/{owner}/{repo}/blob",
                web::delete().to(repositories::delete_blob),
            )
    })
    .bind((bind_addr.as_str(), port))?
    .run()
    .await
}
