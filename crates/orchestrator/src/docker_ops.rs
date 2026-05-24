use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context;
use bollard::Docker;
use bollard::container::{Config, CreateContainerOptions, NetworkingConfig, StartContainerOptions};
use bollard::models::{EndpointSettings, HostConfig, PortBinding};
use uuid::Uuid;

use crate::redis_ops;

/// QUIC port pool for dynamically spawned game servers.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(30);

fn game_image() -> String {
    std::env::var("GAME_IMAGE").unwrap_or_else(|_| "game-server_infonuagique:latest".to_string())
}

fn game_network() -> String {
    std::env::var("GAME_NETWORK").unwrap_or_else(|_| "game-net".to_string())
}

fn port_range() -> (u16, u16) {
    let start = std::env::var("QUIC_PORT_RANGE_START")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7000);
    let end = std::env::var("QUIC_PORT_RANGE_END")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7100);
    (start, end)
}

/// Connect to the Docker daemon via the platform socket.
pub fn connect() -> anyhow::Result<Docker> {
    Docker::connect_with_socket_defaults().context("connect to Docker socket")
}

/// Stop and remove a container by its Docker container ID.
pub async fn stop_container(docker: &Docker, container_id: &str) -> anyhow::Result<()> {
    docker
        .stop_container(container_id, None)
        .await
        .context("stop container")?;
    docker
        .remove_container(container_id, None)
        .await
        .context("remove container")?;
    Ok(())
}

/// Spawn a new game-server container.
///
/// - Picks the next available QUIC port from the configured range.
/// - Creates and starts the container.
/// - Registers a pre-launch Redis entry so the game server can update it.
/// - Polls Redis for `status=ready` with a 30s timeout; removes the container
///   and Redis entry on failure.
pub async fn spawn_server(
    docker: &Docker,
    pool: &deadpool_redis::Pool,
    is_standby: bool,
) -> anyhow::Result<String> {
    let server_id = Uuid::new_v4().to_string();
    let quic_port = pick_free_port(pool).await?;

    // PUBLIC_ADDR is the address clients use to reach this server.
    // Defaults to "localhost" so that port-mapped containers are reachable
    // from the Windows host during local dev. Set the orchestrator env var
    // PUBLIC_ADDR to override (e.g. a public IP for production).
    let public_addr = std::env::var("PUBLIC_ADDR").unwrap_or_else(|_| "localhost".to_string());
    let address = public_addr.clone();
    // Docker-internal DNS name — used by the orchestrator's health checks.
    let internal_addr = format!("game-{server_id}");

    // Write a placeholder entry so wait_for_ready can poll it.
    redis_ops::register_server(
        pool,
        &server_id,
        "", // container_id filled in below
        &address,
        &internal_addr,
        quic_port,
        common::constants::DEFAULT_CAPACITY,
    )
    .await?;

    let env = vec![
        format!("SERVER_ID={server_id}"),
        format!("QUIC_PORT={quic_port}"),
        format!("PUBLIC_ADDR={public_addr}"),
        format!("MAX_PLAYERS={}", common::constants::DEFAULT_CAPACITY),
        std::env::var("REDIS_URL")
            .map(|u| format!("REDIS_URL={u}"))
            .unwrap_or_else(|_| "REDIS_URL=redis://redis:6379".to_string()),
        std::env::var("GATEKEEPER_ADDR")
            .map(|a| format!("GATEKEEPER_ADDR={a}"))
            .unwrap_or_else(|_| "GATEKEEPER_ADDR=gatekeeper:3001".to_string()),
    ];

    // Bind the QUIC UDP port to the host so local clients can reach the container.
    let port_key = format!("{quic_port}/udp");
    let host_config = HostConfig {
        port_bindings: Some(HashMap::from([
            (port_key, Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some(quic_port.to_string()),
            }])),
        ])),
        ..Default::default()
    };

    let network_name = game_network();
    let networking_config = NetworkingConfig {
        endpoints_config: HashMap::from([
            (network_name.clone(), EndpointSettings {
                aliases: Some(vec![format!("game-{server_id}")]),
                ..Default::default()
            }),
        ]),
    };

    let options = CreateContainerOptions {
        name: format!("game-{server_id}"),
        platform: None,
    };

    let config: Config<String> = Config {
        image: Some(game_image()),
        env: Some(env),
        host_config: Some(host_config),
        hostname: Some(format!("game-{server_id}")),
        networking_config: Some(networking_config),
        labels: Some(HashMap::from([
            ("extraction-mmo.role".to_string(), "game-server".to_string()),
            ("extraction-mmo.server-id".to_string(), server_id.clone()),
            ("com.docker.compose.project".to_string(), "extraction-mmo".to_string()),
            ("com.docker.compose.service".to_string(), "game-server".to_string()),
        ])),
        ..Default::default()
    };

    let create_resp = docker
        .create_container(Some(options), config)
        .await
        .context("create container")?;

    let container_id = create_resp.id.clone();

    // Patch the container_id into Redis.
    {
        use deadpool_redis::redis::AsyncCommands;
        let mut conn = pool.get().await.context("Redis pool")?;
        let (): () = conn
            .hset(
                common::redis_keys::server_key(&server_id),
                "container_id",
                &container_id,
            )
            .await
            .context("HSET container_id")?;
    }

    docker
        .start_container(&container_id, None::<StartContainerOptions<String>>)
        .await
        .context("start container")?;

    tracing::info!(
        server_id = %server_id,
        container_id = %container_id,
        quic_port,
        is_standby,
        "Game server container started — waiting for ready"
    );

    // Wait for game server to self-report ready in Redis.
    if let Err(e) = redis_ops::wait_for_ready(pool, &server_id, SPAWN_TIMEOUT).await {
        tracing::error!(server_id = %server_id, "Server failed to become ready: {e}");
        let _ = stop_container(docker, &container_id).await;
        redis_ops::remove_server(pool, &server_id).await?;
        return Err(e);
    }

    if is_standby {
        redis_ops::set_standby(pool, &server_id).await?;
        tracing::info!(server_id = %server_id, "Server registered as standby");
    }

    Ok(server_id)
}

/// Find the lowest QUIC port in the configured range not currently used by any registered server.
async fn pick_free_port(pool: &deadpool_redis::Pool) -> anyhow::Result<u16> {
    let (range_start, range_end) = port_range();
    let servers = redis_ops::list_active_servers(pool).await?;
    let used: std::collections::HashSet<u16> = servers.iter().map(|s| s.quic_port).collect();

    (range_start..=range_end)
        .find(|p| !used.contains(p))
        .ok_or_else(|| anyhow::anyhow!("No free QUIC port in range {range_start}–{range_end}"))
}
