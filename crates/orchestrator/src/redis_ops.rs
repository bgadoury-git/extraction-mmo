use anyhow::Context;
use deadpool_redis::{Config, Pool, Runtime};
use deadpool_redis::redis::AsyncCommands;

use common::redis_keys;

pub fn create_pool(url: &str) -> Pool {
    Config::from_url(url)
        .create_pool(Some(Runtime::Tokio1))
        .expect("failed to create Redis pool")
}

// ---------------------------------------------------------------------------
// Server info
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub id: String,
    pub container_id: String,
    pub address: String,
    /// Docker-internal hostname used by the orchestrator for health checks.
    pub internal_addr: String,
    pub quic_port: u16,
    pub player_count: u32,
    pub capacity: u32,
    pub status: String,
}

/// Return all servers in the active set with their full info.
pub async fn list_active_servers(pool: &Pool) -> anyhow::Result<Vec<ServerInfo>> {
    let mut conn = pool.get().await.context("Redis pool")?;

    let ids: Vec<String> = conn
        .smembers(redis_keys::active_servers_key())
        .await
        .context("SMEMBERS servers:active")?;

    let mut out = Vec::with_capacity(ids.len());

    for id in ids {
        let fields: std::collections::HashMap<String, String> = conn
            .hgetall(redis_keys::server_key(&id))
            .await
            .context("HGETALL")?;

        if fields.is_empty() {
            continue;
        }

        out.push(ServerInfo {
            id: id.clone(),
            container_id: fields.get("container_id").cloned().unwrap_or_default(),
            address: fields.get("address").cloned().unwrap_or_default(),
            internal_addr: fields
                .get("internal_addr")
                .cloned()
                // fall back to address for backwards compat
                .unwrap_or_else(|| fields.get("address").cloned().unwrap_or_default()),
            quic_port: fields
                .get("quic_port")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            player_count: fields
                .get("player_count")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            capacity: fields
                .get("capacity")
                .and_then(|v| v.parse().ok())
                .unwrap_or(common::constants::DEFAULT_CAPACITY),
            status: fields.get("status").cloned().unwrap_or_default(),
        });
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Standby management
// ---------------------------------------------------------------------------

/// Return the ID of the designated standby server, or `None`.
pub async fn get_standby(pool: &Pool) -> anyhow::Result<Option<String>> {
    let mut conn = pool.get().await.context("Redis pool")?;
    let val: Option<String> = conn
        .get(redis_keys::standby_server_key())
        .await
        .context("GET servers:standby")?;
    Ok(val.filter(|s| !s.is_empty()))
}

/// Tag a server as the active standby.
pub async fn set_standby(pool: &Pool, server_id: &str) -> anyhow::Result<()> {
    let mut conn = pool.get().await.context("Redis pool")?;
    let (): () = conn
        .set(redis_keys::standby_server_key(), server_id)
        .await
        .context("SET servers:standby")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Server registration (called by orchestrator after spawning a container)
// ---------------------------------------------------------------------------

pub async fn register_server(
    pool: &Pool,
    server_id: &str,
    container_id: &str,
    address: &str,
    internal_addr: &str,
    quic_port: u16,
    capacity: u32,
) -> anyhow::Result<()> {
    let mut conn = pool.get().await.context("Redis pool")?;
    let key = redis_keys::server_key(server_id);

    let (): () = conn
        .hset_multiple(
            &key,
            &[
                ("container_id", container_id.to_string()),
                ("address", address.to_string()),
                ("internal_addr", internal_addr.to_string()),
                ("quic_port", quic_port.to_string()),
                ("capacity", capacity.to_string()),
                ("player_count", "0".to_string()),
                ("status", "starting".to_string()),
            ],
        )
        .await
        .context("HSET server")?;

    let (): () = conn
        .sadd(redis_keys::active_servers_key(), server_id)
        .await
        .context("SADD servers:active")?;

    Ok(())
}

/// Update the status field of a server hash.
pub async fn set_status(pool: &Pool, server_id: &str, status: &str) -> anyhow::Result<()> {
    let mut conn = pool.get().await.context("Redis pool")?;
    let (): () = conn
        .hset(redis_keys::server_key(server_id), "status", status)
        .await
        .context("HSET status")?;
    Ok(())
}

/// Remove a server from Redis entirely after it has stopped.
pub async fn remove_server(pool: &Pool, server_id: &str) -> anyhow::Result<()> {
    let mut conn = pool.get().await.context("Redis pool")?;
    let key = redis_keys::server_key(server_id);
    let (): () = conn.del(&key).await.context("DEL server")?;
    let (): () = conn
        .srem(redis_keys::active_servers_key(), server_id)
        .await
        .context("SREM servers:active")?;
    Ok(())
}

/// Poll until the server reports `status=ready`, with a timeout.
pub async fn wait_for_ready(
    pool: &Pool,
    server_id: &str,
    timeout: std::time::Duration,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("Timed out waiting for server {server_id} to become ready");
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let mut conn = pool.get().await.context("Redis pool")?;
        let status: Option<String> = conn
            .hget(redis_keys::server_key(server_id), "status")
            .await
            .context("HGET status")?;

        if status.as_deref() == Some("ready") {
            return Ok(());
        }
    }
}
