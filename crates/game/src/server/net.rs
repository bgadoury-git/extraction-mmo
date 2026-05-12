use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use bevy::prelude::Resource;
use bytes::Bytes;
use deadpool_redis::{Config as RedisConfig, Pool, Runtime as RedisRuntime};
use deadpool_redis::redis::AsyncCommands;
use quinn::{Connection, Endpoint, ServerConfig};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use common::packets::{AuthAck, PlayerInput, ValidateToken};
use common::redis_keys;

use super::ServerConfig as GameServerConfig;

// ---------------------------------------------------------------------------
// Shared state between async QUIC tasks and Bevy simulation
// ---------------------------------------------------------------------------

static NEXT_ENTITY_ID: AtomicU32 = AtomicU32::new(1);

/// A command sent from an async connection handler to the Bevy simulation.
pub enum SimCommand {
    PlayerJoined { entity_id: u32, display_name: String },
    PlayerLeft  { entity_id: u32 },
    PlayerInput { entity_id: u32, dx: f32, dy: f32 },
}

/// Thread-safe list of authenticated QUIC connections, used by Bevy systems
/// to broadcast position snapshots without needing an async context
/// (`quinn::Connection::send_datagram` is synchronous).
#[derive(Clone, Default, Resource)]
pub struct ConnectedPlayers(pub Arc<Mutex<Vec<(u32, Connection)>>>);

/// Bevy resource — wraps the receiver end of the simulation command channel.
#[derive(Resource)]
pub struct SimCommandReceiver(pub Mutex<std::sync::mpsc::Receiver<SimCommand>>);

// ---------------------------------------------------------------------------
// QuicServer — owns the endpoint and tracks connected peers
// ---------------------------------------------------------------------------

pub struct QuicServer {
    pub endpoint: Endpoint,
}

pub async fn setup(
    cfg: &GameServerConfig,
    conn_list: ConnectedPlayers,
    cmd_tx: std::sync::mpsc::SyncSender<SimCommand>,
) -> anyhow::Result<QuicServer> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok(); // may already be installed

    let redis_pool = create_redis_pool(&cfg.redis_url)?;

    // Register as "starting".
    register_server(cfg, &redis_pool, "starting").await?;

    let endpoint = bind_quic_endpoint(cfg.quic_port).await?;

    // Accept player connections in a background task.
    let _endpoint_clone = endpoint.clone();
    tokio::spawn(accept_loop(
        _endpoint_clone,
        cfg.server_id.clone(),
        conn_list,
        cmd_tx,
        redis_pool.clone(),
    ));

    // Set status to "ready".
    register_server(cfg, &redis_pool, "ready").await?;
    tracing::info!(
        server_id = %cfg.server_id,
        port = cfg.quic_port,
        "Game server READY"
    );

    // Start Redis heartbeat (every 2 s).
    let pool_clone = redis_pool.clone();
    let server_id = cfg.server_id.clone();
    tokio::spawn(redis_heartbeat(pool_clone, server_id));

    Ok(QuicServer { endpoint })
}

// ---------------------------------------------------------------------------
// Redis helpers
// ---------------------------------------------------------------------------

fn create_redis_pool(url: &str) -> anyhow::Result<Pool> {
    RedisConfig::from_url(url)
        .create_pool(Some(RedisRuntime::Tokio1))
        .context("deadpool-redis create_pool")
}

async fn register_server(
    cfg: &GameServerConfig,
    pool: &Pool,
    status: &str,
) -> anyhow::Result<()> {
    let mut conn = pool.get().await.context("Redis pool error")?;
    let key = redis_keys::server_key(&cfg.server_id);

    let (): () = deadpool_redis::redis::cmd("HSET")
        .arg(&key)
        .arg("address")
        .arg(&cfg.public_addr)
        .arg("quic_port")
        .arg(cfg.quic_port)
        .arg("player_count")
        .arg(0u32)
        .arg("capacity")
        .arg(cfg.max_players)
        .arg("status")
        .arg(status)
        .query_async(&mut *conn)
        .await
        .context("HSET server")?;

    if status == "ready" {
        let (): () = conn
            .sadd(redis_keys::active_servers_key(), &cfg.server_id)
            .await
            .context("SADD servers:active")?;
    }

    Ok(())
}

async fn redis_heartbeat(pool: Pool, server_id: String) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        interval.tick().await;
        match pool.get().await {
            Ok(mut conn) => {
                // player_count is already tracked via INCR/DECR; just touch the key.
                let key = redis_keys::server_key(&server_id);
                let _: Result<(), _> = deadpool_redis::redis::cmd("HSET")
                    .arg(&key)
                    .arg("heartbeat")
                    .arg(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                    )
                    .query_async(&mut *conn)
                    .await;
            }
            Err(e) => tracing::warn!("Redis heartbeat pool error: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// QUIC endpoint
// ---------------------------------------------------------------------------

async fn bind_quic_endpoint(quic_port: u16) -> anyhow::Result<Endpoint> {
    let cert = generate_simple_self_signed(vec!["localhost".to_string()])
        .context("rcgen cert")?;

    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        cert.key_pair.serialize_der(),
    ));

    let tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .context("rustls ServerConfig")?;

    let quic_cfg = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .context("QuicServerConfig")?;

    let endpoint = Endpoint::server(
        ServerConfig::with_crypto(Arc::new(quic_cfg)),
        format!("0.0.0.0:{quic_port}").parse()?,
    )
    .context("Endpoint::server")?;

    Ok(endpoint)
}

async fn accept_loop(
    endpoint: Endpoint,
    server_id: String,
    conn_list: ConnectedPlayers,
    cmd_tx: std::sync::mpsc::SyncSender<SimCommand>,
    redis_pool: Pool,
) {
    while let Some(incoming) = endpoint.accept().await {
        let conn_list = conn_list.clone();
        let cmd_tx = cmd_tx.clone();
        let redis_pool = redis_pool.clone();
        let server_id = server_id.clone();
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => {
                    tracing::info!(remote = %conn.remote_address(), "Player QUIC connection");
                    handle_connection(conn, conn_list, cmd_tx, redis_pool, server_id).await;
                }
                Err(e) => tracing::warn!("QUIC accept error: {e}"),
            }
        });
    }
}

async fn handle_connection(
    conn: Connection,
    conn_list: ConnectedPlayers,
    cmd_tx: std::sync::mpsc::SyncSender<SimCommand>,
    redis_pool: Pool,
    server_id: String,
) {
    let entity_id = NEXT_ENTITY_ID.fetch_add(1, Ordering::Relaxed);

    // ── Auth handshake ──────────────────────────────────────────────────────
    let display_name = match conn.accept_bi().await {
        Ok((mut send, mut recv)) => {
            let data = match recv.read_to_end(4096).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("Failed to read auth stream: {e}");
                    return;
                }
            };
            let name = if let Ok(msg) = bitcode::decode::<ValidateToken>(&data) {
                tracing::info!(token = %msg.token, entity_id, remote = %conn.remote_address(), "Player authenticated");
                msg.token
            } else {
                format!("player_{entity_id}")
            };
            // Reply with the assigned entity_id so the client knows who it is.
            let ack = bitcode::encode(&AuthAck { entity_id });
            if let Err(e) = send.write_all(&Bytes::from(ack)).await {
                tracing::warn!("Failed to send AuthAck: {e}");
                return;
            }
            send.finish().ok();
            name
        }
        Err(e) => {
            tracing::warn!("Failed to accept auth stream: {e}");
            return;
        }
    };

    // Register in the shared connection list so Bevy can broadcast to this player.
    conn_list.0.lock().unwrap().push((entity_id, conn.clone()));

    // Notify the Bevy simulation that a player joined.
    let _ = cmd_tx.send(SimCommand::PlayerJoined {
        entity_id,
        display_name,
    });

    // ── Input receive loop ──────────────────────────────────────────────────
    loop {
        match conn.read_datagram().await {
            Ok(data) => {
                if let Ok(input) = bitcode::decode::<PlayerInput>(&data) {
                    let _ = cmd_tx.send(SimCommand::PlayerInput {
                        entity_id,
                        dx: input.dx,
                        dy: input.dy,
                    });
                }
            }
            Err(e) => {
                tracing::info!(entity_id, remote = %conn.remote_address(), "Player disconnected: {e}");
                break;
            }
        }
    }

    // ── Cleanup ─────────────────────────────────────────────────────────────
    conn_list.0.lock().unwrap().retain(|(id, _)| *id != entity_id);
    let _ = cmd_tx.send(SimCommand::PlayerLeft { entity_id });
    // Decrement the player_count in Redis so the orchestrator and gatekeeper
    // see accurate occupancy and can route new joins correctly.
    match redis_pool.get().await {
        Ok(mut conn) => {
            let key = redis_keys::server_key(&server_id);
            let _: Result<(), _> = deadpool_redis::redis::cmd("HINCRBY")
                .arg(&key)
                .arg("player_count")
                .arg(-1_i64)
                .query_async(&mut *conn)
                .await;
        }
        Err(e) => tracing::warn!("Redis pool error on disconnect: {e}"),
    }}
