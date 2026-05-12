use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{redis_ops, AppState};

#[derive(Deserialize)]
pub struct JoinRequest {
    pub display_name: String,
}

#[derive(Serialize)]
pub struct JoinResponse {
    pub token: String,
    pub server_addr: String,
    pub quic_port: u16,
}

pub async fn handler(
    State(state): State<AppState>,
    Json(body): Json<JoinRequest>,
) -> Result<Json<JoinResponse>, StatusCode> {
    // Basic input validation at the system boundary.
    let name = body.display_name.trim();
    if name.is_empty() || name.len() > 32 {
        return Err(StatusCode::BAD_REQUEST);
    }

    let mut conn = state.redis.get().await.map_err(|e| {
        tracing::error!("Redis pool error during join: {e}");
        StatusCode::SERVICE_UNAVAILABLE
    })?;

    // 1. Find best server (most filled that still has room).
    let server = redis_ops::find_server_for_join(&mut conn)
        .await
        .map_err(|e| {
            tracing::error!("find_server_for_join failed: {e:#}");
            StatusCode::SERVICE_UNAVAILABLE
        })?;

    let Some(server) = server else {
        tracing::warn!("No available server for player '{name}'");
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };

    // 2. Atomically reserve a slot via Lua script.
    let reserved = redis_ops::reserve_slot(&mut conn, &server.id, server.capacity)
        .await
        .map_err(|e| {
            tracing::error!("reserve_slot failed: {e:#}");
            StatusCode::SERVICE_UNAVAILABLE
        })?;

    if !reserved {
        // Server filled up in the window between find and reserve — retry is
        // left to the client (simple approach; good enough for this iteration).
        tracing::warn!("Slot reservation race on server '{}' for player '{name}'", server.id);
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    // 3. Allocate a player ID.
    let player_id = redis_ops::next_player_id(&mut conn)
        .await
        .map_err(|e| {
            tracing::error!("next_player_id failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // 4. Generate token and store session with 1h TTL.
    let token = Uuid::new_v4().to_string();

    redis_ops::store_session(&mut conn, &token, &server.id, player_id)
        .await
        .map_err(|e| {
            tracing::error!("store_session failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    tracing::info!(
        player_id,
        server_id = %server.id,
        "Player '{name}' assigned → {}:{}",
        server.address,
        server.quic_port,
    );

    Ok(Json(JoinResponse {
        token,
        server_addr: server.address,
        quic_port: server.quic_port,
    }))
}
