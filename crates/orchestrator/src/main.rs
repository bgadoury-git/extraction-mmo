mod docker_ops;
mod health;
mod redis_ops;

use std::time::Duration;

use anyhow::Result;
use tracing::info;
use kube::Client;

use common::constants::SPAWN_THRESHOLD;

#[tokio::main]
async fn main() -> Result<()> {
    // Install rustls ring provider for QUIC health checks (must be first!)
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "orchestrator=info".parse().unwrap()),
        )
        .init();

    let redis_url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let redis_pool = loop {
        let pool = redis_ops::create_pool(&redis_url);
        // Try to get a connection to test if Redis is ready
        match pool.get().await {
            Ok(_) => {
                info!("Connected to Redis at {redis_url}");
                break pool;
            }
            Err(e) => {
                tracing::warn!("Waiting for Redis to be ready: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    };

    // Initialize Kubernetes client
    let k8s = Client::try_default().await?;
    info!("Connected to Kubernetes cluster");

    // Ensure at least one standby server is running at startup.
    let standby = redis_ops::get_standby(&redis_pool).await?;
    if standby.is_none() {
        info!("No standby server found — spawning one");
        docker_ops::spawn_server_k8s(&k8s, &redis_pool, true).await?;
    }

    let poll_interval = Duration::from_secs(5);
    let health_interval = Duration::from_secs(10);

    let mut poll_tick = tokio::time::interval(poll_interval);
    let mut health_tick = tokio::time::interval(health_interval);

    loop {
        tokio::select! {
            _ = poll_tick.tick() => {
                if let Err(e) = run_poll_cycle(&k8s, &redis_pool).await {
                    tracing::error!("Poll cycle error: {e}");
                }
            }
            _ = health_tick.tick() => {
                if let Err(e) = health::run_health_checks(&redis_pool).await {
                    tracing::error!("Health check error: {e}");
                }
            }
        }
    }
}

/// Main 5-second poll: manage standby, scale up on high load, drain empties.
async fn run_poll_cycle(
    k8s: &kube::Client,
    pool: &deadpool_redis::Pool,
) -> Result<()> {
    let servers = redis_ops::list_active_servers(pool).await?;
    tracing::debug!(?servers, "All active servers from Redis");

    // Compute empty_servers first — we derive has_standby from it so the
    // check is always based on live data, not a potentially-stale Redis key.
    let empty_servers: Vec<_> = servers
        .iter()
        .filter(|s| s.player_count == 0 && s.status != "draining")
        .collect();
    tracing::debug!(empty_servers = ?empty_servers.iter().map(|s| &s.id).collect::<Vec<_>>(), "Empty servers (player_count == 0, not draining)");

    let has_standby = !empty_servers.is_empty();
    let needs_scale = servers
        .iter()
        .any(|s| s.player_count as f64 / s.capacity as f64 >= SPAWN_THRESHOLD as f64);
    tracing::debug!(has_standby, needs_scale, "Poll state: has_standby, needs_scale");

    // ── Scale up: spawn a new server when any existing server is ≥ 85% full
    //    and there is no idle standby.
    if needs_scale && !has_standby {
        info!("Load threshold reached — spawning new server");
        docker_ops::spawn_server_k8s(k8s, pool, false).await?;
    }

    // ── Ensure exactly 1 standby (promote the emptiest server if needed).
    // Exclude servers already being drained so we don't re-drain them every cycle.
    match empty_servers.len() {
        0 => {
            // No idle server at all — spawn a standby if not already scaling
            // (if we're scaling, the newly spawned server will become the standby).
            if !needs_scale {
                info!("No standby server — spawning one");
                docker_ops::spawn_server_k8s(k8s, pool, true).await?;
            }
        }
        1 => {
            // Exactly one idle server — make sure it's tagged as standby.
            let standby_id = empty_servers[0].id.clone();
            redis_ops::set_standby(pool, &standby_id).await?;
        }
        _ => {
            // More than one empty server — keep the first as standby, drain rest.
            redis_ops::set_standby(pool, &empty_servers[0].id).await?;
            for surplus in &empty_servers[1..] {
                info!(id = %surplus.id, "Draining surplus empty server");
                redis_ops::set_status(pool, &surplus.id, "draining").await?;
                let server_id = surplus.id.clone();
                let pool_clone = pool.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    if let Err(e) = redis_ops::remove_server(&pool_clone, &server_id).await {
                        tracing::error!(id = %server_id, "Failed to remove server from Redis: {e}");
                    }
                });
            }
        }
    }

    Ok(())
}
