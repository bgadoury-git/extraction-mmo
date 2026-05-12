mod quic_validator;
mod redis_ops;
mod routes;

use axum::{
    Router,
    routing::{get, post},
};
use tokio::net::TcpListener;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Shared application state threaded through Axum handlers and the QUIC validator.
#[derive(Clone)]
pub struct AppState {
    pub redis: deadpool_redis::Pool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Install the ring crypto provider for rustls (required by rustls 0.23+).
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .init();

    let redis_url = std::env::var("REDIS_URL")
        .unwrap_or_else(|_| "redis://redis:6379".to_string());

    let redis_pool = redis_ops::create_pool(&redis_url)?;

    let state = AppState { redis: redis_pool };

    // Spawn the internal QUIC validator on port 3001 (game server → gatekeeper).
    let quic_state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = quic_validator::run(quic_state).await {
            tracing::error!("QUIC validator crashed: {e:#}");
        }
    });

    let app = Router::new()
        .route("/join", post(routes::join::handler))
        .route("/health", get(routes::health::handler))
        .with_state(state);

    let listener = TcpListener::bind("0.0.0.0:3000").await?;
    tracing::info!("Gatekeeper HTTP listening on 0.0.0.0:3000");
    axum::serve(listener, app).await?;

    Ok(())
}
