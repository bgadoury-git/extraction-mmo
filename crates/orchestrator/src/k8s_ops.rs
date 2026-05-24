use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context;
use k8s_openapi::api::core::v1::{
    Container, EnvVar, Pod, PodSpec, SecretVolumeSource, Service, ServicePort, ServiceSpec,
    Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{DeleteParams, PostParams};
use kube::{Api, Client};
use uuid::Uuid;

use crate::redis_ops;

const SPAWN_TIMEOUT: Duration = Duration::from_secs(120);
const LB_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Azure typically provisions a LoadBalancer IP within 60–90 s; 3 min is a safe ceiling.
const LB_POLL_TIMEOUT: Duration = Duration::from_secs(180);
/// All dynamically spawned game servers listen on this port (each gets its own public IP).
const QUIC_PORT: u16 = 7000;

// ---------------------------------------------------------------------------
// Config helpers
// ---------------------------------------------------------------------------

pub fn namespace() -> String {
    std::env::var("K8S_NAMESPACE").unwrap_or_else(|_| "extraction-mmo".to_string())
}

fn game_image() -> String {
    std::env::var("GAME_IMAGE")
        .unwrap_or_else(|_| "extractionmmoacr.azurecr.io/game-server:latest".to_string())
}

fn cert_secret_name() -> String {
    std::env::var("CERT_SECRET_NAME").unwrap_or_else(|_| "extraction-mmo-certs".to_string())
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Build an in-cluster Kubernetes client (reads the pod's service-account token).
/// Falls back to `~/.kube/config` when running outside a cluster (local dev).
pub async fn connect() -> anyhow::Result<Client> {
    Client::try_default()
        .await
        .context("create Kubernetes client")
}

// ---------------------------------------------------------------------------
// Server lifecycle
// ---------------------------------------------------------------------------

/// Spawn a new game-server Pod + LoadBalancer Service.
///
/// Flow:
/// 1. Create the Service (LoadBalancer, UDP 7000) — ClusterIP is assigned immediately.
/// 2. Register a Redis placeholder with the ClusterIP as `internal_addr`.
/// 3. Create the Pod.
/// 4. Poll until Azure assigns the external LoadBalancer IP (~60–90 s).
/// 5. Update the Redis `address` field with the real public IP.
/// 6. Wait for the game server to self-report `status=ready` in Redis.
pub async fn spawn_server(
    client: &Client,
    pool: &deadpool_redis::Pool,
    is_standby: bool,
) -> anyhow::Result<String> {
    let server_id = Uuid::new_v4().to_string();
    // Pod/Service names must be DNS-safe; use the first 8 hex chars of the UUID.
    let name = format!("game-{}", &server_id[..8]);
    let ns = namespace();

    let redis_url = std::env::var("REDIS_URL")
        .unwrap_or_else(|_| "redis://redis:6379".to_string());
    let gatekeeper_addr = std::env::var("GATEKEEPER_ADDR")
        .unwrap_or_else(|_| "gatekeeper-quic:3001".to_string());

    // Labels shared by both the Pod and Service (Service selector matches `app`).
    let mut labels: BTreeMap<String, String> = BTreeMap::new();
    labels.insert("app".to_string(), name.clone());
    labels.insert("extraction-mmo/role".to_string(), "game-server".to_string());
    labels.insert("extraction-mmo/server-id".to_string(), server_id.clone());

    // --- 1. Create LoadBalancer Service ---
    let services: Api<Service> = Api::namespaced(client.clone(), &ns);

    let mut selector: BTreeMap<String, String> = BTreeMap::new();
    selector.insert("app".to_string(), name.clone());

    let svc = Service {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            namespace: Some(ns.clone()),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            type_: Some("LoadBalancer".to_string()),
            selector: Some(selector),
            ports: Some(vec![ServicePort {
                port: QUIC_PORT as i32,
                protocol: Some("UDP".to_string()),
                target_port: Some(IntOrString::Int(QUIC_PORT as i32)),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };

    let created_svc = services
        .create(&PostParams::default(), &svc)
        .await
        .context("create game-server Service")?;

    // ClusterIP is assigned synchronously — use it for internal health checks.
    let internal_addr = created_svc
        .spec
        .as_ref()
        .and_then(|s| s.cluster_ip.as_deref())
        .unwrap_or("")
        .to_string();

    // --- 2. Register Redis placeholder (address will be updated after LB IP is ready) ---
    redis_ops::register_server(
        pool,
        &server_id,
        &name, // container_id field repurposed to store pod name
        "pending",
        &internal_addr,
        QUIC_PORT,
        common::constants::DEFAULT_CAPACITY,
    )
    .await?;

    // --- 3. Create Pod ---
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);

    let pod = Pod {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            namespace: Some(ns.clone()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![Container {
                name: "game-server".to_string(),
                image: Some(game_image()),
                env: Some(vec![
                    env_var("SERVER_ID", &server_id),
                    env_var("QUIC_PORT", &QUIC_PORT.to_string()),
                    env_var("PUBLIC_ADDR", "pending"), // updated below once LB IP is known
                    env_var(
                        "MAX_PLAYERS",
                        &common::constants::DEFAULT_CAPACITY.to_string(),
                    ),
                    env_var("REDIS_URL", &redis_url),
                    env_var("GATEKEEPER_ADDR", &gatekeeper_addr),
                ]),
                volume_mounts: Some(vec![VolumeMount {
                    name: "certs".to_string(),
                    mount_path: "/certs".to_string(),
                    read_only: Some(true),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            volumes: Some(vec![Volume {
                name: "certs".to_string(),
                secret: Some(SecretVolumeSource {
                    secret_name: Some(cert_secret_name()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };

    pods.create(&PostParams::default(), &pod)
        .await
        .context("create game-server Pod")?;

    tracing::info!(
        server_id = %server_id,
        pod = %name,
        internal_addr = %internal_addr,
        "Pod and Service created — polling for LoadBalancer IP"
    );

    // --- 4. Poll for external LoadBalancer IP ---
    let lb_ip = wait_for_lb_ip(&services, &name)
        .await
        .context("waiting for LoadBalancer IP")?;

    tracing::info!(server_id = %server_id, lb_ip = %lb_ip, "LoadBalancer IP assigned");

    // --- 5. Update Redis with the real public IP ---
    {
        use deadpool_redis::redis::AsyncCommands;
        let mut conn = pool.get().await.context("Redis pool")?;
        let (): () = conn
            .hset(
                common::redis_keys::server_key(&server_id),
                "address",
                &lb_ip,
            )
            .await
            .context("HSET address")?;
    }

    // --- 6. Wait for game server to self-report ready ---
    if let Err(e) = redis_ops::wait_for_ready(pool, &server_id, SPAWN_TIMEOUT).await {
        tracing::error!(server_id = %server_id, "Server failed to become ready: {e}");
        let _ = remove_server_k8s(client, &name, &ns).await;
        redis_ops::remove_server(pool, &server_id).await?;
        return Err(e);
    }

    if is_standby {
        redis_ops::set_standby(pool, &server_id).await?;
        tracing::info!(server_id = %server_id, "Server registered as standby");
    }

    Ok(server_id)
}

/// Delete the Pod and Service for a game server. Ignores not-found errors.
pub async fn remove_server_k8s(
    client: &Client,
    name: &str,
    ns: &str,
) -> anyhow::Result<()> {
    let dp = DeleteParams::default();
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let services: Api<Service> = Api::namespaced(client.clone(), ns);
    // Ignore errors — the resource may already be gone.
    let _ = pods.delete(name, &dp).await;
    let _ = services.delete(name, &dp).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Poll `service.status.loadBalancer.ingress[0].ip` until Azure assigns the IP.
async fn wait_for_lb_ip(services: &Api<Service>, name: &str) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + LB_POLL_TIMEOUT;
    loop {
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("Timed out waiting for LoadBalancer IP on Service {name}");
        }

        let svc = services.get(name).await.context("get Service")?;
        if let Some(ip) = svc
            .status
            .as_ref()
            .and_then(|s| s.load_balancer.as_ref())
            .and_then(|lb| lb.ingress.as_ref())
            .and_then(|v| v.first())
            .and_then(|i| i.ip.as_deref())
            .filter(|ip| !ip.is_empty())
        {
            return Ok(ip.to_string());
        }

        tokio::time::sleep(LB_POLL_INTERVAL).await;
    }
}

fn env_var(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value: Some(value.to_string()),
        ..Default::default()
    }
}
