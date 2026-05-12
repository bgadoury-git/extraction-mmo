use bitcode::{Decode, Encode};

// ---------------------------------------------------------------------------
// Unreliable datagrams — sent every tick via QUIC unreliable datagrams.
// Only nearby entities (within INTEREST_RADIUS_TILES) are included.
// ---------------------------------------------------------------------------

/// Position and velocity snapshot for a single entity.
/// Sent unreliably every tick; dropped packets are simply skipped.
/// Entity IDs are u32 — never UUIDs — to keep datagrams small.
#[derive(Debug, Clone, Copy, Encode, Decode)]
pub struct PositionSnapshot {
    pub entity_id: u32,
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
}

/// A batch of position snapshots sent to one client per tick.
#[derive(Debug, Clone, Encode, Decode)]
pub struct PositionBatch {
    pub tick: u32,
    pub snapshots: Vec<PositionSnapshot>,
}

// ---------------------------------------------------------------------------
// Reliable stream packets — sent over QUIC reliable streams (ordered).
// ---------------------------------------------------------------------------

/// A player connected to the shard.
#[derive(Debug, Clone, Encode, Decode)]
pub struct PlayerJoined {
    pub entity_id: u32,
    pub display_name: String,
}

/// A player disconnected or was removed from the shard.
#[derive(Debug, Clone, Encode, Decode)]
pub struct PlayerLeft {
    pub entity_id: u32,
}

/// A player picked up an item.
#[derive(Debug, Clone, Encode, Decode)]
pub struct ItemPickup {
    pub entity_id: u32,
    pub item_id: u32,
}

/// Damage was dealt from one entity to another.
#[derive(Debug, Clone, Copy, Encode, Decode)]
pub struct DamageEvent {
    pub source: u32,
    pub target: u32,
    pub amount: f32,
}

/// The result of a player's extraction attempt.
#[derive(Debug, Clone, Copy, Encode, Decode)]
pub struct ExtractionResult {
    pub player_id: u32,
    pub success: bool,
}

// ---------------------------------------------------------------------------
// Envelope — top-level reliable packet discriminant.
// ---------------------------------------------------------------------------

/// All reliable stream payloads wrapped in a single enum for framing.
#[derive(Debug, Clone, Encode, Decode)]
pub enum ReliablePacket {
    PlayerJoined(PlayerJoined),
    PlayerLeft(PlayerLeft),
    ItemPickup(ItemPickup),
    DamageEvent(DamageEvent),
    ExtractionResult(ExtractionResult),
}

// ---------------------------------------------------------------------------
// Auth handshake packets (game server ↔ client)
// ---------------------------------------------------------------------------

/// Sent by the game server to the client after successful authentication.
/// Contains the entity ID assigned to this player for this session.
#[derive(Debug, Clone, Copy, Encode, Decode)]
pub struct AuthAck {
    pub entity_id: u32,
}

/// Player movement input — sent as unreliable datagram from client to server each frame.
/// `dx`/`dy` are in the range [-1, 1]; the server normalises and scales by speed.
#[derive(Debug, Clone, Copy, Encode, Decode)]
pub struct PlayerInput {
    pub dx: f32,
    pub dy: f32,
}

// ---------------------------------------------------------------------------
// Internal QUIC packets (game server ↔ gatekeeper)
// ---------------------------------------------------------------------------

/// Sent by a game server to the gatekeeper to validate a session token.
#[derive(Debug, Clone, Encode, Decode)]
pub struct ValidateToken {
    pub token: String,
}

/// Positive validation response from the gatekeeper.
#[derive(Debug, Clone, Encode, Decode)]
pub struct TokenValid {
    pub server_id: String,
    pub player_id: u32,
}

/// Negative validation response from the gatekeeper.
#[derive(Debug, Clone, Copy, Encode, Decode)]
pub struct TokenInvalid;

/// Discriminated union for gatekeeper validation responses.
#[derive(Debug, Clone, Encode, Decode)]
pub enum ValidationResponse {
    Valid(TokenValid),
    Invalid(TokenInvalid),
}
