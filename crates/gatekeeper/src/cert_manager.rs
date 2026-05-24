use std::collections::BTreeMap;

use anyhow::Context;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::PostParams;
use kube::{Api, Client};

/// Certificates ready to be loaded into a rustls `ServerConfig`.
pub struct GatekeeperCerts {
    /// DER-encoded gatekeeper leaf certificate (signed by the project CA).
    pub cert_der: Vec<u8>,
    /// DER-encoded PKCS#8 private key for the leaf cert.
    pub key_der: Vec<u8>,
}

pub fn namespace() -> String {
    std::env::var("K8S_NAMESPACE").unwrap_or_else(|_| "extraction-mmo".to_string())
}

fn secret_name() -> String {
    std::env::var("CERT_SECRET_NAME").unwrap_or_else(|_| "extraction-mmo-certs".to_string())
}

/// Ensure the TLS cert Secret exists in the cluster.
///
/// **First run**: generates a CA cert + a gatekeeper leaf cert signed by it, then
/// creates the Secret with three fields:
/// - `ca.crt`  — CA cert PEM (game server pods mount this to trust the gatekeeper)
/// - `tls.crt` — Gatekeeper leaf cert DER
/// - `tls.key` — Gatekeeper private key DER
///
/// **Subsequent runs**: reads the existing Secret so the cert is stable across
/// gatekeeper Pod restarts.
pub async fn ensure_certs(client: &Client) -> anyhow::Result<GatekeeperCerts> {
    let ns = namespace();
    let name = secret_name();
    let secrets: Api<Secret> = Api::namespaced(client.clone(), &ns);

    match secrets.get(&name).await {
        Ok(existing) => load_from_secret(&existing, &name),
        Err(kube::Error::Api(ref ae)) if ae.code == 404 => {
            create_cert_secret(&secrets, &name, &ns).await
        }
        Err(e) => Err(e).context(format!("GET Secret {name}")),
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn load_from_secret(secret: &Secret, name: &str) -> anyhow::Result<GatekeeperCerts> {
    let data = secret
        .data
        .as_ref()
        .with_context(|| format!("Secret {name} has no data field"))?;

    let cert_der = data
        .get("tls.crt")
        .with_context(|| format!("Secret {name} missing tls.crt"))?
        .0
        .clone();

    let key_der = data
        .get("tls.key")
        .with_context(|| format!("Secret {name} missing tls.key"))?
        .0
        .clone();

    tracing::info!(secret = %name, "Loaded gatekeeper TLS certs from existing Secret");
    Ok(GatekeeperCerts { cert_der, key_der })
}

async fn create_cert_secret(
    secrets: &Api<Secret>,
    name: &str,
    ns: &str,
) -> anyhow::Result<GatekeeperCerts> {
    let (ca_pem, cert_der, key_der) = generate_certs().context("generate TLS certs")?;

    let mut data: BTreeMap<String, k8s_openapi::ByteString> = BTreeMap::new();
    data.insert(
        "ca.crt".to_string(),
        k8s_openapi::ByteString(ca_pem.into_bytes()),
    );
    data.insert(
        "tls.crt".to_string(),
        k8s_openapi::ByteString(cert_der.clone()),
    );
    data.insert(
        "tls.key".to_string(),
        k8s_openapi::ByteString(key_der.clone()),
    );

    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };

    secrets
        .create(&PostParams::default(), &secret)
        .await
        .context("create cert Secret")?;

    tracing::info!(secret = %name, "Generated CA + gatekeeper certs and created Secret");
    Ok(GatekeeperCerts { cert_der, key_der })
}

/// Generate a CA cert and a gatekeeper leaf cert signed by it.
///
/// Returns `(ca_pem, leaf_cert_der, leaf_key_der)`.
fn generate_certs() -> anyhow::Result<(String, Vec<u8>, Vec<u8>)> {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};

    // --- CA ---
    let mut ca_params = CertificateParams::new(vec!["extraction-mmo-ca".to_string()])
        .context("CA CertificateParams")?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_key = KeyPair::generate().context("CA KeyPair")?;
    let ca_cert = ca_params.self_signed(&ca_key).context("CA self_signed")?;

    // --- Gatekeeper leaf cert (signed by the CA) ---
    let leaf_params = CertificateParams::new(vec![
        "gatekeeper".to_string(),
        "gatekeeper-quic".to_string(),
        "localhost".to_string(),
    ])
    .context("leaf CertificateParams")?;
    let leaf_key = KeyPair::generate().context("leaf KeyPair")?;
    let leaf_cert = leaf_params
        .signed_by(&leaf_key, &ca_cert, &ca_key)
        .context("sign leaf cert")?;

    Ok((ca_cert.pem(), leaf_cert.der().to_vec(), leaf_key.serialize_der()))
}
