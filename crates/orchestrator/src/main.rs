mod health;
mod k8s_ops;
mod redis_ops;

use std::time::Duration;

use anyhow::Result;
use tracing::info;

use common::constants::SPAWN_THRESHOLD;

#[tokio::main]
async fn main() -> Result<()> {
    // Must be the very first call: kube::Client::try_default() connects to the
    // K8s API server over TLS, so the crypto provider must be registered first.
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "orchestrator=info".parse().unwrap()),
        )
        .init();

    let redis_url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let redis_pool = redis_ops::create_pool(&redis_url);
    info!("Connected to Redis at {redis_url}");

    let client = k8s_ops::connect().await?;
    info!("Connected to Kubernetes API");

    // Ensure at least one standby server is running at startup.
    let standby = redis_ops::get_standby(&redis_pool).await?;
    if standby.is_none() {
        info!("No standby server found — spawning one");
        k8s_ops::spawn_server(&client, &redis_pool, true).await?;
    }

    let poll_interval = Duration::from_secs(5);
    let health_interval = Duration::from_secs(10);

    let mut poll_tick = tokio::time::interval(poll_interval);
    let mut health_tick = tokio::time::interval(health_interval);

    loop {
        tokio::select! {
            _ = poll_tick.tick() => {
                if let Err(e) = run_poll_cycle(&client, &redis_pool).await {
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
    client: &kube::Client,
    pool: &deadpool_redis::Pool,
) -> Result<()> {
    let servers = redis_ops::list_active_servers(pool).await?;

    // Compute empty_servers first — we derive has_standby from it so the
    // check is always based on live data, not a potentially-stale Redis key.
    let empty_servers: Vec<_> = servers
        .iter()
        .filter(|s| s.player_count == 0 && s.status != "draining")
        .collect();

    let has_standby = !empty_servers.is_empty();
    let needs_scale = servers
        .iter()
        .any(|s| s.player_count as f64 / s.capacity as f64 >= SPAWN_THRESHOLD as f64);

    // ── Scale up: spawn a new server when any existing server is ≥ 85% full
    //    and there is no idle standby.
    if needs_scale && !has_standby {
        info!("Load threshold reached — spawning new server");
        k8s_ops::spawn_server(client, pool, false).await?;
    }

    // ── Ensure exactly 1 standby (promote the emptiest server if needed).
    // Exclude servers already being drained so we don't re-drain them every cycle.
    match empty_servers.len() {
        0 => {
            // No idle server at all — spawn a standby if not already scaling
            // (if we're scaling, the newly spawned server will become the standby).
            if !needs_scale {
                info!("No standby server — spawning one");
                k8s_ops::spawn_server(client, pool, true).await?;
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
                let client_clone = client.clone();
                let pod_name = surplus.container_id.clone(); // container_id stores pod name
                let server_id = surplus.id.clone();
                let pool_clone = pool.clone();
                let ns = k8s_ops::namespace();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    if pod_name.is_empty() {
                        tracing::warn!(id = %server_id, "No pod name recorded — removing stale Redis entry");
                    } else if let Err(e) = k8s_ops::remove_server_k8s(&client_clone, &pod_name, &ns).await {
                        tracing::error!(pod = %pod_name, "Failed to remove pod: {e}");
                    }
                    if let Err(e) = redis_ops::remove_server(&pool_clone, &server_id).await {
                        tracing::error!(id = %server_id, "Failed to remove server from Redis: {e}");
                    }
                });
            }
        }
    }

    Ok(())
}
