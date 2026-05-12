/// Simulation tick rate in Hz.
pub const TICK_RATE_HZ: u32 = 30;

/// Default maximum players per game server shard.
pub const DEFAULT_CAPACITY: u32 = 128;

/// Fraction of capacity at which the orchestrator spawns a new shard.
pub const SPAWN_THRESHOLD: f32 = 0.85;

/// Spatial interest radius in tiles — only entities within this range receive
/// position snapshots for a given player.
pub const INTEREST_RADIUS_TILES: u32 = 32;

/// Minimum movement (in world units) required before a position update is sent.
/// Updates with delta below this value are skipped to save bandwidth.
pub const POSITION_DELTA_THRESHOLD: f32 = 0.1;
