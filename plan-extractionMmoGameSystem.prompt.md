# Plan: 2D Extraction MMO — Full System

**TL;DR:** A Cargo workspace with 4 crates (`common`, `game`, `gatekeeper`, `orchestrator`), Dockerized, using HTTP/1.1 for login and QUIC everywhere else. The orchestrator dynamically spawns game server containers via the Docker API when shards hit 85% capacity, always keeping one empty standby shard.

---

## Project Layout

```
extraction-mmo/
├── Cargo.toml                    # workspace root
├── docker-compose.yml
├── dockerfiles/
│   ├── Dockerfile.game
│   ├── Dockerfile.gatekeeper
│   └── Dockerfile.orchestrator
└── crates/
    ├── common/        # packets, protocol consts, Redis key helpers
    ├── game/          # Bevy project — features: [client] / [server]
    ├── gatekeeper/    # Axum HTTP + internal QUIC endpoint
    └── orchestrator/  # bollard Docker manager + Redis poll loop
```

---

## Communication Matrix

| From → To | Protocol | Notes |
|---|---|---|
| Client → Gatekeeper | HTTP/1.1 REST | login/dispatch only |
| Client → Game Server | QUIC (quinn) | game data |
| Game Server → Redis | TCP (deadpool-redis) | state reporting |
| Orchestrator → Redis | TCP (deadpool-redis) | load monitoring |
| Orchestrator → Game Server | QUIC (quinn) | health pings |
| Game Server → Gatekeeper | QUIC (quinn) | token validation |

---

## Redis Schema

| Key | Type | Fields |
|---|---|---|
| `server:{id}` | HASH | `address`, `quic_port`, `player_count`, `capacity`, `status` |
| `servers:active` | SET | IDs of ready servers |
| `servers:standby` | STRING | ID of the 0-player standby |
| `session:{token}` | HASH | `server_id`, `expires_at` |

Slot reservation uses a **Lua atomic script** (INCR + check) to prevent race conditions between concurrent `/join` requests.

---

## Phase 0 — Workspace Bootstrap
1. Create `extraction-mmo/` with root `Cargo.toml` declaring workspace members
2. Stub all 4 crates with `cargo new --lib` / `--bin`
3. Pin shared dependency versions in `[workspace.dependencies]`
4. Add `.cargo/config.toml` (opt-level, codegen-units for release), `rustfmt.toml`, `clippy.toml`

## Phase 1 — `common` Crate *(no dependencies between phases, do first)*
All packet types and constants shared across crates.

**Packet types** (serialized with `bitcode`, not JSON):
- **Unreliable datagrams** (every tick): `PositionSnapshot { entity_id: u32, x: f32, y: f32, vx: f32, vy: f32 }` — interest-managed, only nearby entities sent
- **Reliable streams** (events): `PlayerJoined`, `PlayerLeft`, `ItemPickup`, `DamageEvent { source: u32, target: u32, amount: f32 }`, `ExtractionResult { player_id: u32, success: bool }`

**Protocol constants:** `TICK_RATE_HZ = 30`, `DEFAULT_CAPACITY = 128`, `SPAWN_THRESHOLD = 0.85`, `INTEREST_RADIUS_TILES = 32`

**Redis key helpers:** `fn session_key(token: &str)`, `fn server_key(id: &str)`, etc.

**Bandwidth discipline (enforced here):**
- Entity IDs are `u32`, never UUIDs in packets
- Delta threshold: skip position update if movement < 0.1 units
- Spatial interest: only send entities within `INTEREST_RADIUS_TILES` (quadtree/spatial hash in game server)

## Phase 2 — `gatekeeper` Crate *(depends on Phase 1)*
Axum HTTP server on port 3000 + quinn QUIC endpoint on port 3001.

**HTTP endpoints:**
- `POST /join` body `{ display_name }` → `{ token: String, server_addr: String, quic_port: u16 }`
- `GET /health` → 200

**`POST /join` logic:**
1. Generate UUID v4 token
2. Query Redis: find a `ready` server below capacity — prefer most-filled first (pack players, free standby)
3. Atomically reserve slot via Lua script
4. Store `session:{token}` with 1h TTL
5. Return token + server QUIC address

**QUIC endpoint (internal):** listens on 3001 for `ValidateToken { token }` requests from game servers — responds with `TokenValid { server_id, player_id }` or `TokenInvalid`.

Key files: `crates/gatekeeper/src/main.rs`, `routes/join.rs`, `redis_ops.rs`, `quic_validator.rs`

## Phase 3 — `game` Crate (Bevy) *(depends on Phases 1 + 2)*
One Bevy project, two binaries via feature flags.

**`crates/game/Cargo.toml` features:**
```toml
[features]
client = ["bevy/...", "dep:reqwest"]
server = ["dep:quinn", "dep:deadpool-redis", "dep:tokio"]
```
Compile guard in `main.rs` prevents enabling both simultaneously.

**Server build** (`--features server --no-default-features`):
- `MinimalPlugins` (headless — no window, no renderer, no audio)
- `FixedUpdate` at `TICK_RATE_HZ` for simulation tick
- **Startup sequence:** read env (`SERVER_ID`, `QUIC_PORT`, `MAX_PLAYERS`, `REDIS_URL`, `GATEKEEPER_ADDR`) → register in Redis `status=starting` → bind quinn QUIC endpoint → set `status=ready`
- **Per-tick:** process queued input events, advance simulation (movement, collision, extraction zones), run spatial interest query, batch-serialize `PositionSnapshot` datagrams, send unreliable to each player's visible set
- **Event handling** (reliable streams): pickup, damage, extraction, disconnect
- **Redis heartbeat** (every 2s): update `player_count` in `server:{id}`
- **Graceful shutdown:** set `status=draining`, stop accepting new connections, wait for all players to disconnect or timeout (30s), delete Redis key

Key files: `crates/game/src/server/mod.rs`, `net.rs`, `simulation.rs`, `interest.rs`

**Client build** (`--features client --no-default-features`):
- `DefaultPlugins` with a window
- **Login screen** (Bevy UI or `bevy_egui`): enter display name → `POST /join` via `reqwest` → store `GameSession { token, server_addr, quic_port }` resource
- **Transition to game:** connect quinn QUIC client to server → send token in first reliable stream for auth
- **Game loop:** send input datagrams every tick, receive `PositionSnapshot` bursts, interpolate remote entities, handle reliable event streams
- **UI:** minimap, inventory panel, extraction timer

Key files: `crates/game/src/client/login.rs`, `net.rs`, `interpolation.rs`

## Phase 4 — `orchestrator` Crate *(depends on Phase 1)*
Tokio service; manages container lifecycle.

**Startup:**
1. Connect to Docker socket via `bollard` (`/var/run/docker.sock`)
2. Connect to Redis
3. If no standby server in Redis → call `spawn_server(standby=true)`

**Poll loop every 5s:**
1. Read all `servers:active` entries from Redis
2. If any server ≥ 85% full AND no spare server → `spawn_server(standby=false)`
3. Ensure exactly 1 standby (0-player) server tagged in `servers:standby`
4. If 2+ servers are empty → pick oldest, set `status=draining` → stop container after 30s grace

**`spawn_server()` via `bollard`:**
1. Generate `SERVER_ID` (UUID), pick available QUIC port from a configured range
2. `docker.create_container()` with `game-server` image, env vars, `game-net` network
3. `docker.start_container()`
4. Poll Redis for `status=ready` with 30s timeout; error and remove container on timeout

**QUIC health check:** ping each server's QUIC endpoint every 10s; mark non-responsive servers `status=draining` after 2 missed pings.

Key files: `crates/orchestrator/src/main.rs`, `docker_ops.rs`, `redis_ops.rs`, `health.rs`

## Phase 5 — Docker *(parallel with Phases 2–4)*
**`Dockerfile.game`** — multi-stage Rust build, `--features server`, expose `$QUIC_PORT`
**`Dockerfile.gatekeeper`** — multi-stage Rust build, expose 3000 (HTTP) + 3001 (QUIC)
**`Dockerfile.orchestrator`** — multi-stage Rust build, mounts Docker socket

**`docker-compose.yml` services:**
- `redis` — `redis:7-alpine`, port 6379, named volume
- `gatekeeper` — `depends_on: redis`, port 3000 exposed to host
- `orchestrator` — `depends_on: [redis, gatekeeper]`, volume `- /var/run/docker.sock:/var/run/docker.sock`
- **No** `game-server` service — orchestrator spawns these at runtime on `game-net`

All services join bridge network `game-net` so containers can reach each other by name.

**Dev TLS for QUIC:** game server uses `rcgen` to generate a self-signed cert on first boot, writes it to a shared Docker volume `/certs/`. Client reads the CA cert from the same volume and trusts it. In `debug_assertions` builds, cert verification can be skipped entirely via a custom `rustls` verifier.

## Phase 6 — Integration & Verification
1. `docker compose up --build orchestrator` → verify redis, gatekeeper, orchestrator start; orchestrator logs spawning the standby server
2. Check Redis: `redis-cli HGETALL server:{id}` shows `status=ready`
3. Run client binary → enter name → confirm `200 OK` from gatekeeper with a token and server address
4. Client connects to game server via QUIC → server logs token validation success
5. Simulate 110 player joins (load test script against `/join`) → verify Redis shows a second server spawned and one remains as standby
6. Stop all connections to one server → verify orchestrator drains and stops the container, standby is re-seeded
7. Kill a game server container manually → verify health check marks it draining within 20s

---

## Key Dependencies Summary

| Crate | Key deps |
|---|---|
| `common` | `serde`, `bitcode` |
| `gatekeeper` | `axum`, `tokio`, `quinn`, `rustls`, `rcgen`, `deadpool-redis`, `uuid` |
| `game` | `bevy`, `quinn` (server feat), `reqwest` (client feat), `deadpool-redis`, `bitcode` |
| `orchestrator` | `tokio`, `bollard`, `deadpool-redis`, `quinn`, `uuid` |

---

## Decisions
- **Token-only auth** — UUID v4 issued by Gatekeeper, 1h TTL, no passwords
- **128 players/shard** default, 85% threshold, all env-configurable
- **`bitcode`** for binary serialization in hot path — no JSON in game packets
- **Dev TLS** — `rcgen` self-signed cert, shared Docker volume; cert verification skipped in debug builds
- **Orchestrator spawns servers** dynamically — Docker Compose does not pre-define game servers
- **Packing strategy** — Gatekeeper assigns players to the most-filled ready server (not standby) to keep one server free
- **Out of scope for this iteration:** persistent user accounts, matchmaking by region/skill, server migration, Kubernetes

---

## Open Considerations
1. **Extraction instance model:** Extraction MMOs typically run per-raid instances (30–60 players in a session, then it ends). Should each game server host one instance or multiple? This affects server lifecycle significantly — want to confirm before implementing the simulation layer.
2. **Port range:** Orchestrator needs a fixed QUIC port pool (e.g., 7000–7100) for game servers. Ensure this range is accessible between containers and doesn't conflict with host services.
3. **Bevy version:** Bevy 0.15 is the latest stable — confirm this is acceptable, as breaking changes between versions affect plugin APIs substantially.
