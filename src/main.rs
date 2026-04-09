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

    // Bind to all interfaces by default so other containers on the docker
    // network can reach this service. NEVER bind to 127.0.0.1 in a container -
    // it would only be reachable from inside the container itself.
    let bind_addr = env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0".to_string());

    println!("git-agent listening on http://{bind_addr}:{port}");

    HttpServer::new(|| {
        let cors = create_cors();

        App::new()
            .wrap(cors)
            .service(health)
            .route(
                "/repositories",
                web::get().to(repositories::list_repositories),
            )
            .route(
                "/repositories",
                web::post().to(repositories::create_repository),
            )
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
                "/repositories/{owner}/{repo}/tree",
                web::get().to(repositories::get_tree),
            )
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
