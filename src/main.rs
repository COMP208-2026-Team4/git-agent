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

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    dotenv().ok();

    let port: u16 = env::var("PORT")
        .unwrap_or_else(|_| "6025".to_string())
        .parse()
        .expect("PORT must be a valid port number");

    println!("git-agent running on http://0.0.0.0:{port}");

    HttpServer::new(|| {
        let cors = Cors::permissive(); // tighten in production

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
