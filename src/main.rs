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

    println!("git-agent running on http://0.0.0.0:{port}");

    HttpServer::new(|| {
        let cors = create_cors();

        App::new()
            .wrap(cors)
            .service(health)
            .route(
                "/repositories",
                web::post().to(repositories::create_repository),
            )
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}
