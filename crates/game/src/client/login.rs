use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task};
use futures_lite::future;
use serde::{Deserialize, Serialize};

use super::{GameSession, GameState};

pub struct LoginPlugin;

impl Plugin for LoginPlugin {
    fn build(&self, app: &mut App) {
        app
            .add_systems(OnEnter(GameState::Login), setup_login_ui)
            .add_systems(OnExit(GameState::Login), teardown_login_ui)
            .add_systems(
                Update,
                (handle_join_button, poll_join_task).run_if(in_state(GameState::Login)),
            );
    }
}

// ---------------------------------------------------------------------------
// UI markers
// ---------------------------------------------------------------------------

#[derive(Component)]
struct LoginRoot;

#[derive(Component)]
struct NameInput;

#[derive(Component)]
struct JoinButton;

#[derive(Component)]
struct StatusText;

// ---------------------------------------------------------------------------
// Async join task
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct JoinRequest {
    display_name: String,
}

#[derive(Deserialize)]
struct JoinResponse {
    token: String,
    server_addr: String,
    quic_port: u16,
}

#[derive(Component)]
struct JoinTask(Task<Result<JoinResponse, String>>);

// ---------------------------------------------------------------------------
// Systems
// ---------------------------------------------------------------------------

fn setup_login_ui(mut commands: Commands) {
    commands.spawn((
        Camera2d,
        LoginRoot,
    ));

    commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                row_gap: Val::Px(16.0),
                ..default()
            },
            LoginRoot,
        ))
        .with_children(|parent| {
            // Title
            parent.spawn((
                Text::new("Extraction MMO"),
                TextFont { font_size: 40.0, ..default() },
            ));

            // Name input placeholder (static text — full text input needs bevy_ui_text_input)
            parent.spawn((
                Text::new("Enter name: Player1"),
                TextFont { font_size: 24.0, ..default() },
                NameInput,
            ));

            // Join button
            parent
                .spawn((
                    Button,
                    Node {
                        padding: UiRect::all(Val::Px(12.0)),
                        ..default()
                    },
                    BackgroundColor(Color::srgb(0.2, 0.6, 0.2)),
                    JoinButton,
                ))
                .with_children(|btn| {
                    btn.spawn((
                        Text::new("Join"),
                        TextFont { font_size: 24.0, ..default() },
                    ));
                });

            // Status line
            parent.spawn((
                Text::new(""),
                TextFont { font_size: 18.0, ..default() },
                TextColor(Color::srgb(1.0, 0.4, 0.4)),
                StatusText,
            ));
        });
}

fn teardown_login_ui(mut commands: Commands, roots: Query<Entity, With<LoginRoot>>) {
    for entity in &roots {
        commands.entity(entity).despawn_related::<Children>();
        commands.entity(entity).despawn();
    }
}

fn handle_join_button(
    mut commands: Commands,
    interaction_query: Query<&Interaction, (Changed<Interaction>, With<JoinButton>)>,
    name_query: Query<&Text, With<NameInput>>,
    mut status_query: Query<&mut Text, (With<StatusText>, Without<NameInput>)>,
    gatekeeper_url: Option<Res<GatekeeperUrl>>,
) {
    for interaction in &interaction_query {
        if *interaction != Interaction::Pressed {
            continue;
        }

        let display_name = name_query
            .single()
            .map(|t| t.0.replace("Enter name: ", "").trim().to_string())
            .unwrap_or_else(|_| "Player1".to_string());

        if display_name.is_empty() {
            if let Ok(mut text) = status_query.single_mut() {
                text.0 = "Name cannot be empty".to_string();
            }
            return;
        }

        let url = gatekeeper_url
            .as_ref()
            .map(|r| r.0.clone())
            .unwrap_or_else(|| "http://localhost:3000".to_string());

        let task_pool = AsyncComputeTaskPool::get();
        let task = task_pool.spawn(async move {
            // reqwest requires a Tokio runtime; AsyncComputeTaskPool uses async-executor
            // (no Tokio context), so we build a dedicated single-threaded runtime here.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;

            rt.block_on(async {
                let client = reqwest::Client::new();
                let resp = client
                    .post(format!("{url}/join"))
                    .json(&JoinRequest { display_name })
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;

                if resp.status().is_success() {
                    resp.json::<JoinResponse>().await.map_err(|e| e.to_string())
                } else {
                    Err(format!("Server returned {}", resp.status()))
                }
            })
        });

        commands.spawn(JoinTask(task));
    }
}

fn poll_join_task(
    mut commands: Commands,
    mut tasks: Query<(Entity, &mut JoinTask)>,
    mut status_query: Query<&mut Text, With<StatusText>>,
    mut next_state: ResMut<NextState<GameState>>,
) {
    for (entity, mut task) in &mut tasks {
        if let Some(result) = future::block_on(future::poll_once(&mut task.0)) {
            commands.entity(entity).despawn();
            match result {
                Ok(resp) => {
                    commands.insert_resource(GameSession {
                        token: resp.token,
                        server_addr: resp.server_addr,
                        quic_port: resp.quic_port,
                    });
                    next_state.set(GameState::Connecting);
                }
                Err(e) => {
                    if let Ok(mut text) = status_query.single_mut() {
                        text.0 = format!("Join failed: {e}");
                    }
                }
            }
        }
    }
}

/// Optional resource to override the gatekeeper base URL (e.g. from env).
#[derive(Resource)]
pub struct GatekeeperUrl(pub String);
