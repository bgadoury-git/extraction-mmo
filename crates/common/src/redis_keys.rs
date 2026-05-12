/// Redis key for a game server's metadata hash.
/// Fields: `address`, `quic_port`, `player_count`, `capacity`, `status`.
pub fn server_key(id: &str) -> String {
    format!("server:{id}")
}

/// Redis key for the set of active (ready) server IDs.
pub fn active_servers_key() -> &'static str {
    "servers:active"
}

/// Redis key for the string holding the current standby server ID.
pub fn standby_server_key() -> &'static str {
    "servers:standby"
}

/// Redis key for a session token hash.
/// Fields: `server_id`, `expires_at`.
pub fn session_key(token: &str) -> String {
    format!("session:{token}")
}
