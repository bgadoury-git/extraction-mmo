# Docker → AKS Kubernetes Migration Plan

## Decisions
- **Cloud**: AKS (Azure Kubernetes Service) — region: `canadaeast`
- **Registry**: ACR — `extractionmmoacr.azurecr.io`
- **Orchestrator refactor**: replace `bollard` (Docker API) with `kube` crate (Kubernetes API)
- **Scale**: Small — <100 concurrent players, 1–5 game servers
- **CI/CD**: Out of scope for now

## Architecture
| Component | K8s Resource | Exposure |
|---|---|---|
| redis | StatefulSet + PVC (5 GiB) | ClusterIP :6379 (internal only) |
| gatekeeper | Deployment (1 replica) | LoadBalancer TCP :3000 (public) + ClusterIP UDP :3001 (internal, name: `gatekeeper-quic`) |
| orchestrator | Deployment (1 replica) | None (internal controller) |
| game-server | Pod (spawned dynamically) | LoadBalancer UDP :7000 per server (unique public IP each) |

## Key Challenges
1. **Orchestrator**: Docker API (`bollard`) → Kubernetes API (`kube` crate)
2. **Cert distribution**: Docker named volume → K8s Secret (gatekeeper writes at startup)
3. **Game server exposure**: LoadBalancer Service per server, poll for public IP before registering in Redis
4. **DNS**: `gatekeeper:3001` (Docker) → `gatekeeper-quic:3001` (Kubernetes)
5. **PUBLIC_ADDR**: static `localhost` → dynamic LB IP from `service.status.loadBalancer.ingress`

---

## Phase 1: Azure & AKS Setup ✅ DONE

```bash
# Install tools
winget install Microsoft.AzureCLI Kubernetes.kubectl Microsoft.Azure.Kubelogin

# Login (use personal Microsoft account, NOT UQAC — university tenant has no subscription)
az login
az account set --subscription <subscription-id>

# Register resource providers (required on new subscriptions, ~2 min)
az provider register --namespace Microsoft.ContainerRegistry
az provider register --namespace Microsoft.ContainerService
az provider register --namespace Microsoft.Compute
az provider register --namespace Microsoft.Network
az provider show --namespace Microsoft.ContainerRegistry --query registrationState

# Create resources
az group create --name rg-extraction-mmo --location canadaeast
az acr create --name extractionmmoacr --resource-group rg-extraction-mmo --sku Basic
az aks create \
  --name aks-extraction-mmo \
  --resource-group rg-extraction-mmo \
  --node-count 1 \
  --node-vm-size Standard_B2als_v2 \
  --attach-acr extractionmmoacr \
  --generate-ssh-keys
# NOTE: Standard_B2s (v1) unavailable in canadaeast — use Standard_B2als_v2 (2 vCPU, 4 GB, ~$15/mo)

# Configure kubectl
az aks get-credentials --name aks-extraction-mmo --resource-group rg-extraction-mmo
kubectl get nodes  # expect: Ready
```

---

## Phase 2: Orchestrator Code Refactor

Replace `bollard` (Docker API) with `kube` + `k8s-openapi` (Kubernetes API).

### `crates/orchestrator/Cargo.toml`
- Remove: `bollard`
- Add: `kube = { version = "0.98", features = ["runtime"] }`, `k8s-openapi = { version = "0.24", features = ["v1_32"] }`

### `crates/orchestrator/src/docker_ops.rs` → rename to `k8s_ops.rs`
Rewrite the following functions:

- **`spawn_server()`**
  1. Create a `Pod` via Kubernetes API with env vars: `SERVER_ID`, `QUIC_PORT=7000`, `REDIS_URL`, `GATEKEEPER_ADDR`, `PUBLIC_ADDR=<lb_ip_placeholder>`
  2. Create a `Service` (type `LoadBalancer`, UDP :7000 → pod :7000)
  3. Poll `service.status.loadBalancer.ingress[0].ip` until Azure assigns the public IP
  4. Store `address=<lb_ip>`, `quic_port=7000` in Redis (`server:{id}` hash)

- **`remove_server()`**: Delete Pod + Service by name

- **`health_check()`**: Keep existing QUIC ping, targeting pod's cluster-internal DNS

### `crates/orchestrator/src/lib.rs`
- Update `mod docker_ops` → `mod k8s_ops`

### `crates/orchestrator/src/main.rs`
- Remove env vars: `GAME_NETWORK`, `CERTS_VOLUME`
- Add env vars: `K8S_NAMESPACE` (= `extraction-mmo`), `CERT_SECRET_NAME` (= `extraction-mmo-certs`)
- Update `GAME_IMAGE` to `extractionmmoacr.azurecr.io/game-server:latest`
- Update `GATEKEEPER_ADDR` default to `gatekeeper-quic:3001`

---

## Phase 3: Cert Distribution Refactor

**Current**: orchestrator writes CA cert to a Docker named volume; gatekeeper and game servers mount it.  
**New**: gatekeeper generates the CA cert at startup and writes it to a Kubernetes Secret; orchestrator mounts that Secret into each game server Pod.

### `crates/gatekeeper/Cargo.toml`
- Add: `kube = { version = "0.98", features = ["runtime"] }`, `k8s-openapi = { version = "0.24", features = ["v1_32"] }`

### Gatekeeper startup logic
1. Check if Secret `extraction-mmo-certs` exists in namespace `extraction-mmo`
2. If not: generate CA cert with `rcgen`, create the Secret via kube API
3. If yes: skip (cert already distributed)

### `crates/orchestrator/src/k8s_ops.rs` — `spawn_server()`
- Read Secret `extraction-mmo-certs` from Kubernetes API
- Mount it as a volume into the game server Pod spec (e.g. at `/certs`)

---

## Phase 4: Kubernetes Manifests

Create a `k8s/` directory in the project root with the following structure:

```
k8s/
  namespace.yaml
  redis/
    statefulset.yaml        # redis:7-alpine, PVC 5 GiB
    service.yaml            # ClusterIP :6379
  gatekeeper/
    deployment.yaml         # 1 replica, env vars
    service-http.yaml       # LoadBalancer TCP :3000  (public — for clients)
    service-quic.yaml       # ClusterIP UDP :3001     (internal — for game servers, name: gatekeeper-quic)
    serviceaccount.yaml
    rbac.yaml               # permissions: create/get/update Secrets
  orchestrator/
    deployment.yaml         # 1 replica, env vars
    serviceaccount.yaml
    rbac.yaml               # permissions: create/delete Pods & Services, read Secrets
```

### Environment variables in Deployments
| Variable | Value |
|---|---|
| `REDIS_URL` | `redis://redis:6379` |
| `GATEKEEPER_ADDR` | `gatekeeper-quic:3001` |
| `GAME_IMAGE` | `extractionmmoacr.azurecr.io/game-server:latest` |
| `K8S_NAMESPACE` | `extraction-mmo` |
| `CERT_SECRET_NAME` | `extraction-mmo-certs` |

---

## Phase 5: Image Build & Push

Build **after** completing Phases 2, 3, and 4 so images contain the refactored code.

```bash
# Authenticate to ACR
az acr login --name extractionmmoacr

# Build (linux/amd64 — AKS nodes are Linux)
docker build --platform linux/amd64 -f dockerfiles/Dockerfile.gatekeeper  -t extractionmmoacr.azurecr.io/gatekeeper:latest .
docker build --platform linux/amd64 -f dockerfiles/Dockerfile.orchestrator -t extractionmmoacr.azurecr.io/orchestrator:latest .
docker build --platform linux/amd64 -f dockerfiles/Dockerfile.game         -t extractionmmoacr.azurecr.io/game-server:latest .

# Push
docker push extractionmmoacr.azurecr.io/gatekeeper:latest
docker push extractionmmoacr.azurecr.io/orchestrator:latest
docker push extractionmmoacr.azurecr.io/game-server:latest

# Verify
az acr repository list --name extractionmmoacr --output table
```

> First build is slow (~10–20 min per image). Subsequent builds are faster due to cargo-chef layer caching.

---

## Phase 6: Deploy to AKS

```bash
# Apply manifests in dependency order
kubectl apply -f k8s/namespace.yaml
kubectl apply -f k8s/redis/
kubectl apply -f k8s/gatekeeper/
kubectl apply -f k8s/orchestrator/

# Check status
kubectl get pods -n extraction-mmo
kubectl get svc  -n extraction-mmo   # wait for EXTERNAL-IP on gatekeeper-http
kubectl get pvc  -n extraction-mmo   # redis PVC should be Bound
```

---

## Phase 7: Networking Verification

- AKS automatically manages NSG rules for `LoadBalancer` type Services — no manual rules needed.
- After `kubectl apply`, Azure provisions public IPs for:
  - `gatekeeper-http` (TCP :3000) — used by game clients to join
  - Each game server Service (UDP :7000) — provisioned on demand by orchestrator

```powershell
# Get gatekeeper public IP
kubectl get svc gatekeeper-http -n extraction-mmo -o jsonpath='{.status.loadBalancer.ingress[0].ip}'
```

---

## Verification

```powershell
# 1. Test join endpoint
Invoke-WebRequest -Uri "http://20.104.144.109:3000/join" -Method POST `
  -Body '{"display_name":"test_player"}' -ContentType "application/json" `
  -UseBasicParsing | Select-Object -ExpandProperty Content
# Expected: {"token":"...","server_addr":"...","quic_port":7000}

# 2. Connect game client to returned addr:port → successful QUIC handshake

# 3. Load test — watch orchestrator spawn game servers
# Run stress binary pointed at 20.104.144.109:3000
kubectl get pods -n extraction-mmo -w   # observe new game-server pods appearing

# 4. Check orchestrator logs
kubectl logs -n extraction-mmo deployment/orchestrator

# 5. Verify Redis persistence survives pod restart
kubectl delete pod -n extraction-mmo -l app=redis
kubectl get pvc -n extraction-mmo   # still Bound after restart
```

---

## Phase 8: Client Online Connection

Updated the game client to connect to the live AKS gatekeeper instead of localhost.

### `crates/game/src/client/mod.rs`
- Read `GATEKEEPER_URL` env var at startup; default = `http://20.104.144.109:3000`
- Insert it as `GatekeeperUrl` resource before plugins run

### `crates/game/src/client/login.rs`
- Added `NameBuffer` resource (default `"Player1"`) to track keyboard input
- Name field now shows the live buffer with a `_` cursor
- Added `handle_text_input` system: `KeyboardInput` events update `NameBuffer` (printable chars append, `Backspace` deletes)
- `handle_join_button` reads `Res<NameBuffer>` directly instead of parsing the `Text` component

```powershell
# Run client (connects to AKS by default)
cargo run -p game --features client

# Or override the gatekeeper URL
$env:GATEKEEPER_URL = "http://20.104.144.109:3000"
cargo run -p game --features client