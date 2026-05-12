use anyhow::Context;
use deadpool_redis::{Config, Pool, Runtime};
use deadpool_redis::redis::AsyncCommands;
use common::redis_keys;

pub fn create_pool(url: &str) -> anyhow::Result<Pool> {
    Config::from_url(url)
        .create_pool(Some(Runtime::Tokio1))
        .context("failed to create Redis pool")
}

// ---------------------------------------------------------------------------
// Server discovery
// ---------------------------------------------------------------------------

pub struct ServerInfo {
    pub id: String,
    pub address: String,
    pub quic_port: u16,
    pub capacity: u32,
}

/// Return the most-filled ready server that still has room, or `None` if all
/// servers are full / unavailable.
///
/// Packing players onto the fullest server keeps a free standby shard open.
pub async fn find_server_for_join(
    conn: &mut deadpool_redis::Connection,
) -> anyhow::Result<Option<ServerInfo>> {
    let server_ids: Vec<String> = conn
        .smembers(redis_keys::active_servers_key())
        .await
        .context("SMEMBERS servers:active")?;

    let mut candidates: Vec<(u32, ServerInfo)> = Vec::new();

    for id in server_ids {
        let fields: std::collections::HashMap<String, String> = conn
            .hgetall(redis_keys::server_key(&id))
            .await
            .context("HGETALL server")?;

        let status = fields.get("status").map(String::as_str).unwrap_or("");
        if status != "ready" {
            continue;
        }

        let player_count: u32 = fields
            .get("player_count")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let capacity: u32 = fields
            .get("capacity")
            .and_then(|v| v.parse().ok())
            .unwrap_or(common::constants::DEFAULT_CAPACITY);
        let address = fields.get("address").cloned().unwrap_or_default();
        let quic_port: u16 = fields
            .get("quic_port")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        if player_count < capacity {
            candidates.push((player_count, ServerInfo { id, address, quic_port, capacity }));
        }
    }

    // Most-filled first.
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(candidates.into_iter().next().map(|(_, s)| s))
}

// ---------------------------------------------------------------------------
// Slot reservation (Lua — atomic INCR guarded by capacity check)
// ---------------------------------------------------------------------------

const RESERVE_SLOT_SCRIPT: &str = r#"
local status = redis.call('HGET', KEYS[1], 'status')
if status ~= 'ready' then return 0 end
local count = tonumber(redis.call('HGET', KEYS[1], 'player_count') or '0')
if count >= tonumber(ARGV[1]) then return 0 end
redis.call('HINCRBY', KEYS[1], 'player_count', 1)
return 1
"#;

/// Atomically increment `player_count` on the server if it is still ready and
/// below capacity. Returns `true` when a slot was successfully reserved.
pub async fn reserve_slot(
    conn: &mut deadpool_redis::Connection,
    server_id: &str,
    capacity: u32,
) -> anyhow::Result<bool> {
    let result: i64 = deadpool_redis::redis::cmd("EVAL")
        .arg(RESERVE_SLOT_SCRIPT)
        .arg(1_i32)
        .arg(redis_keys::server_key(server_id))
        .arg(capacity)
        .query_async(&mut **conn)
        .await
        .context("EVAL reserve_slot")?;
    Ok(result == 1)
}

// ---------------------------------------------------------------------------
// Session management
// ---------------------------------------------------------------------------

/// Allocate a monotonically-increasing player ID via Redis INCR.
pub async fn next_player_id(
    conn: &mut deadpool_redis::Connection,
) -> anyhow::Result<u32> {
    let id: i64 = conn
        .incr("player:id_counter", 1_i64)
        .await
        .context("INCR player:id_counter")?;
    Ok(id as u32)
}

/// Store `session:{token}` with a 1-hour TTL.
pub async fn store_session(
    conn: &mut deadpool_redis::Connection,
    token: &str,
    server_id: &str,
    player_id: u32,
) -> anyhow::Result<()> {
    let key = redis_keys::session_key(token);
    let expires_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + 3600;

    let (): () = conn.hset_multiple(
        &key,
        &[
            ("server_id", server_id.to_owned()),
            ("player_id", player_id.to_string()),
            ("expires_at", expires_at.to_string()),
        ],
    )
    .await
    .context("HSET session")?;

    let (): () = conn.expire(&key, 3600)
        .await
        .context("EXPIRE session")?;

    Ok(())
}

/// Look up a session token and return `(server_id, player_id)` if it exists.
pub async fn validate_token(
    conn: &mut deadpool_redis::Connection,
    token: &str,
) -> anyhow::Result<Option<(String, u32)>> {
    let key = redis_keys::session_key(token);
    let fields: std::collections::HashMap<String, String> =
        conn.hgetall(&key).await.context("HGETALL session")?;

    let server_id = match fields.get("server_id") {
        Some(s) => s.clone(),
        None => return Ok(None),
    };
    let player_id: u32 = fields
        .get("player_id")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    Ok(Some((server_id, player_id)))
}
