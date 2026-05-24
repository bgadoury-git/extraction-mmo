use std::sync::Arc;

use anyhow::Context;
use quinn::{Endpoint, ServerConfig};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use common::packets::{TokenInvalid, TokenValid, ValidateToken, ValidationResponse};

use crate::{redis_ops, AppState};

pub async fn run(state: AppState, certs: crate::cert_manager::GatekeeperCerts) -> anyhow::Result<()> {
    let endpoint = make_server_endpoint(certs).context("failed to build QUIC endpoint")?;
    tracing::info!("QUIC validator listening on 0.0.0.0:3001");

    while let Some(incoming) = endpoint.accept().await {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(incoming, state).await {
                tracing::warn!("QUIC validator connection error: {e:#}");
            }
        });
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// TLS / endpoint setup
// ---------------------------------------------------------------------------

fn make_server_endpoint(certs: crate::cert_manager::GatekeeperCerts) -> anyhow::Result<Endpoint> {
    let cert_der = CertificateDer::from(certs.cert_der);
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certs.key_der));

    let server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .context("rustls ServerConfig")?;

    let quic_server_config =
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)
            .context("QuicServerConfig")?;

    let server_config = ServerConfig::with_crypto(Arc::new(quic_server_config));
    let endpoint = Endpoint::server(server_config, "0.0.0.0:3001".parse()?)
        .context("Endpoint::server")?;

    Ok(endpoint)
}

// ---------------------------------------------------------------------------
// Connection / stream handling
// ---------------------------------------------------------------------------

async fn handle_connection(
    incoming: quinn::Incoming,
    state: AppState,
) -> anyhow::Result<()> {
    let conn = incoming.await.context("QUIC handshake")?;
    tracing::debug!(remote = %conn.remote_address(), "QUIC validator: new connection");

    loop {
        match conn.accept_bi().await {
            Ok((send, recv)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_stream(send, recv, &state).await {
                        tracing::warn!("QUIC stream error: {e:#}");
                    }
                });
            }
            Err(quinn::ConnectionError::ApplicationClosed(_)) => break,
            Err(quinn::ConnectionError::LocallyClosed) => break,
            Err(e) => return Err(e.into()),
        }
    }

    Ok(())
}

async fn handle_stream(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    state: &AppState,
) -> anyhow::Result<()> {
    // Limit request size to 4 KiB — tokens are small strings.
    let data = recv
        .read_to_end(4096)
        .await
        .context("read ValidateToken")?;

    let request: ValidateToken =
        bitcode::decode(&data).context("decode ValidateToken")?;

    let mut conn = state
        .redis
        .get()
        .await
        .context("Redis pool error")?;

    let result = redis_ops::validate_token(&mut conn, &request.token)
        .await
        .context("validate_token Redis")?;

    let response = match result {
        Some((server_id, player_id)) => {
            ValidationResponse::Valid(TokenValid { server_id, player_id })
        }
        None => ValidationResponse::Invalid(TokenInvalid),
    };

    let response_bytes = bitcode::encode(&response);
    send.write_all(&response_bytes)
        .await
        .context("write ValidationResponse")?;
    send.finish().ok();

    Ok(())
}
