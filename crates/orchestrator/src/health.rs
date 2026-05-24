use std::collections::HashMap;
use std::sync::Arc;

use crate::redis_ops;

/// How many consecutive missed pings before a server is marked draining.
const MAX_MISSED_PINGS: u8 = 2;

/// Per-server missed-ping counter, keyed by server_id.
static MISSED: std::sync::LazyLock<std::sync::Mutex<HashMap<String, u8>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Ping every active server's QUIC endpoint; mark non-responsive ones draining
/// after `MAX_MISSED_PINGS` consecutive failures.
pub async fn run_health_checks(pool: &deadpool_redis::Pool) -> anyhow::Result<()> {
    let servers = redis_ops::list_active_servers(pool).await?;

    let mut tasks = Vec::new();

    for server in servers {
        if server.status == "draining" || server.status == "starting" {
            continue;
        }

            let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            // Use the ClusterIP stored in Redis as internal_addr for health checks.
            let addr = format!("{}:{}", server.internal_addr, server.quic_port);
            let alive = ping_quic(&addr).await;

            // Scope the mutex lock so it is dropped before any await.
            let should_drain = {
                let mut missed = MISSED.lock().unwrap();
                if alive {
                    missed.remove(&server.id);
                    false
                } else {
                    let count = missed.entry(server.id.clone()).or_insert(0);
                    *count += 1;
                    tracing::warn!(
                        id = %server.id,
                        missed = *count,
                        "QUIC health check failed"
                    );
                    *count >= MAX_MISSED_PINGS
                }
            };

            if should_drain {
                tracing::error!(id = %server.id, "Server unresponsive — marking draining");
                let _ = redis_ops::set_status(&pool, &server.id, "draining").await;
            }
        }));
    }

    for t in tasks {
        let _ = t.await;
    }

    Ok(())
}

/// Open a QUIC connection to `addr` and immediately close it.
/// Returns `true` if the handshake succeeds.
async fn ping_quic(addr: &str) -> bool {
    // Resolve the hostname (Docker DNS) to a SocketAddr.
    let server_addr = match tokio::net::lookup_host(addr).await {
        Ok(mut addrs) => {
            // Prefer IPv4 to match the client endpoint bound to 0.0.0.0.
            let collected: Vec<_> = addrs.collect();
            match collected.iter().find(|a| a.is_ipv4()).or_else(|| collected.first()) {
                Some(a) => *a,
                None => return false,
            }
        }
        Err(_) => return false,
    };

    let Ok(endpoint) = make_ping_endpoint() else {
        return false;
    };

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let conn = endpoint
            .connect(server_addr, "localhost")?
            .await?;
        conn.close(0u32.into(), b"ping");
        Ok::<_, anyhow::Error>(())
    })
    .await;

    matches!(result, Ok(Ok(())))
}

fn make_ping_endpoint() -> anyhow::Result<quinn::Endpoint> {
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerifier))
        .with_no_client_auth();
    let quic_cfg = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?;
    let client_cfg = quinn::ClientConfig::new(Arc::new(quic_cfg));
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_cfg);
    Ok(endpoint)
}

// ---------------------------------------------------------------------------
// Dev cert verifier — identical to game client's NoVerifier
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
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
