pub mod input;
pub mod interpolation;
pub mod login;
pub mod net;

use bevy::prelude::*;
use rustls;

use input::ClientInputPlugin;
use login::LoginPlugin;
use net::ClientNetPlugin;
use interpolation::InterpolationPlugin;

/// The game session received after a successful `/join`.
#[derive(Resource, Clone)]
pub struct GameSession {
    pub token: String,
    pub server_addr: String,
    pub quic_port: u16,
}

/// Top-level app states.
#[derive(States, Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum GameState {
    #[default]
    Login,
    Connecting,
    InGame,
}

pub fn run() {
    // Install the ring crypto provider as the process-level default for rustls.
    // Required when multiple providers are available (ring + aws-lc-rs via reqwest).
    let _ = rustls::crypto::ring::default_provider().install_default();

    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "Extraction MMO".to_string(),
                resolution: bevy::window::WindowResolution::new(1280_u32, 720_u32),
                ..default()
            }),
            ..default()
        }))
        .add_plugins(avian2d::PhysicsPlugins::default())
        .init_state::<GameState>()
        .add_plugins(LoginPlugin)
        .add_plugins(ClientNetPlugin)
        .add_plugins(ClientInputPlugin)
        .add_plugins(InterpolationPlugin)
        .run();
}
