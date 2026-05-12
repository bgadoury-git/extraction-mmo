use bevy::prelude::*;

use common::constants::POSITION_DELTA_THRESHOLD;

use super::net::{MyEntityId, PositionBatchReceived};
use super::GameState;

pub struct InterpolationPlugin;

impl Plugin for InterpolationPlugin {
    fn build(&self, app: &mut App) {
        app
            .add_systems(OnEnter(GameState::InGame), spawn_floor)
            .add_systems(
                Update,
                (
                    spawn_remote_players,
                    interpolate_remote_players,
                )
                    .run_if(in_state(GameState::InGame)),
            );
    }
}

// ---------------------------------------------------------------------------
// Components
// ---------------------------------------------------------------------------

/// Tracks a remote (server-side) entity on the client.
#[derive(Component)]
pub struct RemotePlayer {
    pub entity_id: u32,
    pub target: Vec2,
    pub prev: Vec2,
}

// ---------------------------------------------------------------------------
// Systems
// ---------------------------------------------------------------------------

/// Marker for entities that belong to the in-game scene.
#[derive(Component)]
pub struct GameSceneRoot;

/// Spawn a camera and a visual floor mesh when entering InGame.
fn spawn_floor(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
) {
    commands.spawn((Camera2d, GameSceneRoot));
    commands.spawn((
        Mesh2d(meshes.add(Rectangle::new(4000.0, 32.0))),
        MeshMaterial2d(materials.add(ColorMaterial::from_color(Color::srgb(0.25, 0.22, 0.18)))),
        Transform::from_translation(Vec3::new(0.0, -300.0, 0.0)),
        GameSceneRoot,
    ));
}

/// Spawn a circle for each new entity_id seen in position batches.
/// Own player is rendered green; other players are blue.
fn spawn_remote_players(
    mut commands: Commands,
    mut events: MessageReader<PositionBatchReceived>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    existing: Query<&RemotePlayer>,
    my_id: Option<Res<MyEntityId>>,
) {
    let my_entity_id = my_id.map(|r| r.0);
    for batch in events.read() {
        for snap in &batch.0.snapshots {
            let already_exists = existing.iter().any(|r| r.entity_id == snap.entity_id);
            if !already_exists {
                let pos = Vec2::new(snap.x, snap.y);
                let is_me = my_entity_id == Some(snap.entity_id);
                let color = if is_me {
                    Color::srgb(0.2, 1.0, 0.2) // green = local player
                } else {
                    Color::srgb(0.2, 0.6, 1.0) // blue = other players
                };
                commands.spawn((
                    RemotePlayer {
                        entity_id: snap.entity_id,
                        target: pos,
                        prev: pos,
                    },
                    Mesh2d(meshes.add(Circle::new(16.0))),
                    MeshMaterial2d(materials.add(ColorMaterial::from_color(color))),
                    Transform::from_translation(pos.extend(0.0)),
                ));
            }
        }
    }
}

/// Smooth-step each remote player toward its latest received position.
fn interpolate_remote_players(
    mut events: MessageReader<PositionBatchReceived>,
    mut query: Query<(&mut RemotePlayer, &mut Transform)>,
    time: Res<Time>,
) {
    // Apply latest snapshot targets.
    for batch in events.read() {
        for snap in &batch.0.snapshots {
            for (mut remote, _) in &mut query {
                if remote.entity_id == snap.entity_id {
                    let new_pos = Vec2::new(snap.x, snap.y);
                    if (new_pos - remote.target).length() > POSITION_DELTA_THRESHOLD {
                        remote.prev = remote.target;
                        remote.target = new_pos;
                    }
                }
            }
        }
    }

    // Lerp toward target every frame.
    let alpha = (time.delta_secs() * 15.0).min(1.0);
    for (remote, mut transform) in &mut query {
        let current = transform.translation.truncate();
        let next = current.lerp(remote.target, alpha);
        transform.translation = next.extend(transform.translation.z);
    }
}
