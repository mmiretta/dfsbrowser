mod api;
mod auth;
mod chunks;
mod config;
mod session;
mod snapshot;

use actix_web::middleware::{from_fn, DefaultHeaders};
use actix_web::{web, App, HttpServer};
use config::Settings;
use session::SessionStore;
use std::sync::Arc;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct AppState {
    pub sessions: Arc<SessionStore>,
    pub settings: Arc<Settings>,
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dfs_browser=info,actix_web=warn".into()),
        )
        .init();

    let settings = match Settings::from_env() {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::error!("configuration error: {e:#}");
            std::process::exit(2);
        }
    };

    if settings.basic_auth.is_none() {
        tracing::warn!(
            "no DFS_BROWSER_AUTH_USER/DFS_BROWSER_AUTH_PASSWORD set: anyone who can reach \
             {} can use this server. Keep it on localhost or put it behind an authenticating proxy.",
            settings.bind
        );
    }
    if settings.allowed_endpoints.is_empty() {
        tracing::warn!(
            "DFS_BROWSER_ALLOWED_ENDPOINTS is empty: users may make this server connect to any \
             S3 endpoint. Set it to the endpoint(s) of your DFS bucket."
        );
    } else {
        tracing::info!(
            "allowed S3 endpoints: {}",
            settings.allowed_endpoints.join(", ")
        );
    }

    let sessions = Arc::new(SessionStore::new(settings.session_ttl));
    sessions.clone().spawn_reaper();

    let bind = settings.bind.clone();
    tracing::info!("dfs-browser {VERSION} listening on http://{bind}");

    HttpServer::new(move || {
        let state = web::Data::new(AppState {
            sessions: sessions.clone(),
            settings: settings.clone(),
        });
        App::new()
            .app_data(state)
            .app_data(web::JsonConfig::default().limit(64 * 1024))
            .app_data(web::PayloadConfig::default().limit(settings.max_file_bytes as usize))
            .wrap(from_fn(auth::basic_auth))
            .wrap(
                DefaultHeaders::new()
                    .add(("X-Content-Type-Options", "nosniff"))
                    .add(("Referrer-Policy", "no-referrer"))
                    .add(("X-Frame-Options", "DENY")),
            )
            .route("/", web::get().to(api::index))
            .route("/healthz", web::get().to(api::healthz))
            .route("/api/config", web::get().to(api::config))
            .route("/api/login", web::post().to(api::login))
            .route("/api/logout", web::post().to(api::logout))
            .route("/api/ls", web::get().to(api::ls))
            .route("/api/stat", web::get().to(api::stat))
            .route("/api/versions", web::get().to(api::versions))
            .route("/api/stats", web::get().to(api::stats))
            .route("/api/download", web::get().to(api::download))
            .route("/api/upload", web::post().to(api::upload))
    })
    .bind(&bind)?
    .run()
    .await
}
