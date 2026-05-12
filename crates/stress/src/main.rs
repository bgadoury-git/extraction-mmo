use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use common::packets::{AuthAck, PlayerInput, ValidateToken};
use quinn::{ClientConfig, Endpoint};
use serde::{Deserialize, Serialize};
use tracing::{error, info};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Usage: stress [players] [gatekeeper_url]
/// Defaults: 10 players, http://localhost:3000
fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("stress=info".parse().unwrap()),
        )
        .init();

    let players: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    let gatekeeper = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "http://localhost:3000".to_string());

    info!(players, %gatekeeper, "Starting stress test");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(run(players, gatekeeper));
}

// ---------------------------------------------------------------------------
// HTTP types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct JoinRequest {
    display_name: String,
}

#[derive(Deserialize)]
struct JoinResponse {
    token: String,
    server_addr: String,
    quic_port: u16,
}

// ---------------------------------------------------------------------------
// Counters
// ---------------------------------------------------------------------------

static CONNECTED: AtomicU32 = AtomicU32::new(0);
static FAILED: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

async fn run(players: u32, gatekeeper: String) {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let http = Arc::new(reqwest::Client::new());
    let mut handles = Vec::with_capacity(players as usize);

    for i in 0..players {
        let http = http.clone();
        let gk = gatekeeper.clone();
        handles.push(tokio::spawn(async move {
            let name = format!("bot_{i:03}");
            match simulate_player(name.clone(), http, gk).await {
                Ok(()) => {
                    CONNECTED.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    FAILED.fetch_add(1, Ordering::Relaxed);
                    error!(name, "failed: {e:#}");
                }
            }
        }));
        // Stagger logins slightly so the server isn't hit all at once.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    for h in handles {
        let _ = h.await;
    }

    let ok = CONNECTED.load(Ordering::Relaxed);
    let fail = FAILED.load(Ordering::Relaxed);
    info!(ok, fail, "Stress test complete");
}

// ---------------------------------------------------------------------------
// Single simulated player
// ---------------------------------------------------------------------------

async fn simulate_player(name: String, http: Arc<reqwest::Client>, gatekeeper: String) -> Result<()> {
    // ── 1. HTTP /join ───────────────────────────────────────────────────────
    let resp = http
        .post(format!("{gatekeeper}/join"))
        .json(&JoinRequest { display_name: name.clone() })
        .send()
        .await
        .context("/join request")?;

    if !resp.status().is_success() {
        anyhow::bail!("/join returned {}", resp.status());
    }

    let join: JoinResponse = resp.json().await.context("parse /join response")?;
    info!(name, token = %join.token, addr = %join.server_addr, port = join.quic_port, "Joined");

    // ── 2. QUIC connect ─────────────────────────────────────────────────────
    let endpoint = make_endpoint().context("make_endpoint")?;

    let addr_str = format!("{}:{}", join.server_addr, join.quic_port);
    let server_addr = std::net::ToSocketAddrs::to_socket_addrs(&addr_str.as_str())
        .context("resolve")?
        .find(|a| a.is_ipv4())
        .context("no IPv4 addr")?;

    let conn = endpoint
        .connect(server_addr, "localhost")
        .context("connect")?
        .await
        .context("handshake")?;

    // ── 3. Auth handshake ────────────────────────────────────────────────────
    let (mut send, mut recv) = conn.open_bi().await.context("open_bi")?;
    let payload = bitcode::encode(&ValidateToken { token: join.token });
    send.write_all(&Bytes::from(payload)).await.context("write token")?;
    send.finish().ok();

    let ack_data = recv.read_to_end(64).await.context("read AuthAck")?;
    let ack = bitcode::decode::<AuthAck>(&ack_data).context("decode AuthAck")?;
    info!(name, entity_id = ack.entity_id, "Authenticated");

    // ── 4. Send random input for 10 s, then disconnect ───────────────────────
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut tick = tokio::time::interval(Duration::from_millis(33)); // ~30 Hz

    let directions: &[(f32, f32)] = &[
        (1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0),
        (1.0, 1.0), (-1.0, 1.0), (1.0, -1.0), (-1.0, -1.0),
    ];
    let mut dir_idx = ack.entity_id as usize % directions.len();

    loop {
        tick.tick().await;
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        // Rotate direction every ~1 s (30 ticks).
        if tick.period().as_millis() > 0 {
            dir_idx = (dir_idx + 1) % directions.len();
        }
        let (dx, dy) = directions[dir_idx % directions.len()];
        let data = Bytes::from(bitcode::encode(&PlayerInput { dx, dy }));
        if conn.send_datagram(data).is_err() {
            break;
        }
    }

    conn.close(0u32.into(), b"stress done");
    info!(name, entity_id = ack.entity_id, "Disconnected cleanly");
    Ok(())
}

// ---------------------------------------------------------------------------
// QUIC endpoint — skip cert verification (server uses self-signed rcgen cert)
// ---------------------------------------------------------------------------

fn make_endpoint() -> Result<Endpoint> {
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerifier))
        .with_no_client_auth();

    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_secs(5)));
    transport.max_idle_timeout(Some(Duration::from_secs(30).try_into()?));

    let quic_cfg = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?;
    let mut client_cfg = ClientConfig::new(Arc::new(quic_cfg));
    client_cfg.transport_config(Arc::new(transport));

    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_cfg);
    Ok(endpoint)
}

// ---------------------------------------------------------------------------
// Cert verifier
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct NoVerifier;

impl rustls::client::danger::ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer,
        _intermediates: &[rustls::pki_types::CertificateDer],
        _server_name: &rustls::pki_types::ServerName,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
