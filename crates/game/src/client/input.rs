use bevy::prelude::*;
use bytes::Bytes;

use common::packets::PlayerInput;

use super::net::ServerConnection;
use super::GameState;

pub struct ClientInputPlugin;

impl Plugin for ClientInputPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, send_input.run_if(in_state(GameState::InGame)));
    }
}

fn send_input(
    keys: Res<ButtonInput<KeyCode>>,
    conn: Option<Res<ServerConnection>>,
) {
    let Some(conn) = conn else { return };

    let mut dx = 0.0_f32;
    let mut dy = 0.0_f32;
    if keys.pressed(KeyCode::ArrowLeft)  || keys.pressed(KeyCode::KeyA) { dx -= 1.0; }
    if keys.pressed(KeyCode::ArrowRight) || keys.pressed(KeyCode::KeyD) { dx += 1.0; }
    if keys.pressed(KeyCode::ArrowUp)    || keys.pressed(KeyCode::KeyW) { dy += 1.0; }
    if keys.pressed(KeyCode::ArrowDown)  || keys.pressed(KeyCode::KeyS) { dy -= 1.0; }

    // Only send when a key is held — avoids spamming zero-input datagrams.
    if dx == 0.0 && dy == 0.0 {
        return;
    }

    let data = Bytes::from(bitcode::encode(&PlayerInput { dx, dy }));
    if let Err(e) = conn.0.send_datagram(data) {
        tracing::warn!("send_datagram (input): {e}");
    }
}
