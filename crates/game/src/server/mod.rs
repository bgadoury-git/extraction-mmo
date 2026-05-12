pub mod interest;
pub mod net;
pub mod simulation;
pub mod char_controller;

use std::sync::Arc;

use bevy::prelude::*;
use bevy::app::ScheduleRunnerPlugin;
use bevy::transform::TransformPlugin;
use tokio::runtime::Runtime;
use tokio::sync::Mutex;

use common::constants::TICK_RATE_HZ;

use crate::server::net::{ConnectedPlayers, SimCommandReceiver, QuicServer};
use crate::server::simulation::SimulationPlugin;

/// Env-driven configuration read at startup.
pub struct ServerConfig {
    pub server_id: String,
    /// Externally-reachable hostname/IP registered in Redis for clients to connect to.
    pub public_addr: String,
    pub quic_port: u16,
    pub max_players: u32,
    pub redis_url: String,
    pub gatekeeper_addr: String,
}

impl ServerConfig {
    pub fn from_env() -> Self {
        let server_id = std::env::var("SERVER_ID")
            .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());
        // PUBLIC_ADDR is the address clients will connect to.
        // Defaults to "localhost" for local dev; Docker sets it to the
        // container's externally-reachable hostname or IP.
        let public_addr = std::env::var("PUBLIC_ADDR")
            .unwrap_or_else(|_| "localhost".to_string());
        Self {
            server_id,
            public_addr,
            quic_port: std::env::var("QUIC_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(7000),
            max_players: std::env::var("MAX_PLAYERS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(common::constants::DEFAULT_CAPACITY),
            redis_url: std::env::var("REDIS_URL")
                .unwrap_or_else(|_| "redis://redis:6379".to_string()),
            gatekeeper_addr: std::env::var("GATEKEEPER_ADDR")
                .unwrap_or_else(|_| "gatekeeper:3001".to_string()),
        }
    }
}

/// Bevy resource wrapping the shared QUIC server handle.
#[derive(Resource)]
pub struct QuicServerHandle(pub Arc<Mutex<QuicServer>>);

/// Bevy resource wrapping the Tokio runtime used for async ops on the server.
#[derive(Resource)]
pub struct TokioRuntime(pub Arc<Runtime>);

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cfg = ServerConfig::from_env();
    let rt = Arc::new(Runtime::new().expect("failed to create Tokio runtime"));

    // Shared state between async QUIC tasks and Bevy systems.
    let conn_list = ConnectedPlayers::default();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::sync_channel::<net::SimCommand>(1024);

    // Bootstrap: register in Redis as "starting", bind QUIC, then set "ready".
    let quic_server = rt.block_on(async {
        net::setup(&cfg, conn_list.clone(), cmd_tx).await.expect("QUIC setup failed")
    });
    let quic_handle = Arc::new(Mutex::new(quic_server));

    let tick_duration = std::time::Duration::from_secs_f64(1.0 / TICK_RATE_HZ as f64);

    App::new()
        .add_plugins(
            MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(tick_duration)),
        )
        .add_plugins(TransformPlugin)
        .add_plugins(SimulationPlugin)
        .insert_resource(QuicServerHandle(quic_handle))
        .insert_resource(TokioRuntime(rt))
        .insert_resource(conn_list)
        .insert_resource(SimCommandReceiver(std::sync::Mutex::new(cmd_rx)))
        .run();
}
