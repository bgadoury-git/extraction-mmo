# Step-by-Step: Migrate from Local Docker to Kubernetes (Docker Desktop)

This guide walks you through migrating your project from running in local Docker containers (via Docker Compose) to running in the local Kubernetes cluster provided by Docker Desktop. Helm is NOT used.

---

## 1. Preparation
- **Enable Kubernetes in Docker Desktop:**
  - Open Docker Desktop > Settings > Kubernetes > Enable Kubernetes.
  - Wait for the cluster to be ready (check for green status).

- **Review Current Setup:**
  - Identify all services, ports, environment variables, volumes, and dependencies in your `docker-compose.yml` and Dockerfiles.

---

## 2. Build Docker Images
- For each service, build the Docker image:
  - `docker build -t <service-name>:latest <service-directory>`
- Images can be used directly by Kubernetes on Docker Desktop (no push needed).

---

## 3. Write Kubernetes Manifests
---

## 3b. Refactor Orchestrator to Spawn Game Servers as Kubernetes Pods

By default, the orchestrator spawns game servers as Docker containers. To run fully in Kubernetes, refactor the orchestrator to create game server pods using the Kubernetes API:

- Add the following dependencies to `crates/orchestrator/Cargo.toml`:
  ```toml
  kube = { version = "0.91", features = ["runtime", "derive"] }
  k8s-openapi = "0.21"
  serde_json = "1.0"
  ```
- Implement a new function (e.g., `spawn_server_k8s`) in `docker_ops.rs` that creates a `Pod` using the `kube` client, with the same environment variables and ports as the original container.
- Update `main.rs` to initialize a Kubernetes client and call `spawn_server_k8s` instead of the Docker-based function.
- The orchestrator will now dynamically create and manage game server pods in the Kubernetes cluster, rather than Docker containers.

This enables true dynamic scaling of game servers within Kubernetes.
  
---

## 3c. Expose Game Server Pods for External Client Access

To allow your local client (.exe) to connect to a game server pod:

- The orchestrator now creates a Kubernetes Service (type: NodePort) for each game server pod it spawns.
- The orchestrator stores the NodePort and external address (e.g., localhost) in Redis for each game server.
- The client queries the gatekeeper, which provides the address/port of an available game server.
- The client connects directly to the game server using the provided address and NodePort.

**Example Service YAML (created dynamically by orchestrator):**
```yaml
apiVersion: v1
kind: Service
metadata:
  name: game-server-<id>
spec:
  type: NodePort
  selector:
    extraction-mmo.server-id: <id>
  ports:
    - protocol: UDP
      port: <quic-port>
      targetPort: <quic-port>
      nodePort: <assigned-node-port>
```

**Summary of Flow:**
- Client requests a game server from the gatekeeper.
- Gatekeeper queries Redis for an available game server’s external address/port.
- Client connects to the game server via the NodePort.

This ensures your local client can connect and play the game with dynamically spawned game server pods.
  - `k8s/<service>-deployment.yaml`
  - `k8s/<service>-service.yaml`

---

## 4. Convert docker-compose.yml to K8s (Optional: Use Kompose)
- Install Kompose: https://kompose.io/
- Run: `kompose convert -f docker-compose.yml -o k8s/`
- Review and edit generated YAMLs for accuracy and best practices.

created 

INFO Kubernetes file "k8s\\gatekeeper-service.yaml" created 
INFO Kubernetes file "k8s\\redis-service.yaml" created 
INFO Kubernetes file "k8s\\gatekeeper-deployment.yaml" created 
INFO Kubernetes file "k8s\\certs-persistentvolumeclaim.yaml" created 
INFO Kubernetes file "k8s\\orchestrator-deployment.yaml" created 
INFO Kubernetes file "k8s\\redis-deployment.yaml" created 
INFO Kubernetes file "k8s\\redis-data-persistentvolumeclaim.yaml" created 

---

## 5. Deploy to Kubernetes
- Apply all manifests:
  - `kubectl apply -f k8s/`
- Check status:
  - `kubectl get pods`
  - `kubectl get svc`

---

## 6. Access Services
- For web UIs or APIs, expose via NodePort or LoadBalancer in the Service YAML.
- For internal-only services, use ClusterIP (default).
- To access a service:
  - `kubectl port-forward svc/<service-name> <local-port>:<service-port>`

---

## 7. Verify & Troubleshoot
- Check pod logs: `kubectl logs <pod-name>`
- Describe resources: `kubectl describe pod <pod-name>`
- Ensure all services are running and accessible.

### Common Issues & Fixes

- **Orchestrator CrashLoopBackOff with rustls error:**
  - If you see a panic about `CryptoProvider` not being set, you must call `rustls::crypto::ring::default_provider().install_default();` as the very first line in your orchestrator's `main()` function, before any async or library code.
  - **Reason:** rustls requires a crypto provider to be installed before any cryptographic operations. If not set early, the orchestrator will panic at startup.
  - Move this call to the top of `main()` to ensure correct initialization.

- **ImagePullBackOff / ErrImagePull:**
  - Double-check your image names in the manifests. For official images (like Redis), use e.g. `redis:7-alpine`.
  - If using custom images, ensure they are built and available locally or in a registry accessible to your cluster.

- **PersistentVolumeClaim Pending or Fails to Bind:**
  - For local-path provisioner (Docker Desktop, Rancher Desktop), only `ReadWriteOnce` (not `ReadOnlyMany` or `ReadWriteMany`) is supported for accessModes.
  - If you need to change accessModes, you must delete and recreate the PVC:
    1. `kubectl delete pvc <name>`
    2. Edit the manifest to use `ReadWriteOnce`.
    3. `kubectl apply -f <manifest>`

- **Pods Pending:**
  - Check for missing or unbound PVCs, image pull errors, or resource constraints.
  - Use `kubectl describe pod <pod-name>` for detailed status and events.

- **PVC spec is immutable:**
  - Only `resources.requests` and `volumeAttributesClassName` can be changed after creation. For other changes, delete and recreate the PVC.

---

## 7b. RBAC for Orchestrator Pod Management

If the orchestrator needs to create or manage pods in the cluster, you must grant it the correct Kubernetes permissions:

1. **Create an RBAC manifest (k8s/orchestrator-rbac.yaml):**

    ```yaml
    apiVersion: rbac.authorization.k8s.io/v1
    kind: Role
    metadata:
      name: orchestrator-pod-manager
      namespace: default
    rules:
      - apiGroups: [""]
        resources: ["pods"]
        verbs: ["create", "get", "list", "watch", "delete"]
    ---
    apiVersion: rbac.authorization.k8s.io/v1
    kind: RoleBinding
    metadata:
      name: orchestrator-pod-manager-binding
      namespace: default
    subjects:
      - kind: ServiceAccount
        name: default
        namespace: default
    roleRef:
      kind: Role
      name: orchestrator-pod-manager
      apiGroup: rbac.authorization.k8s.io
    ```

2. **Apply the RBAC manifest:**

    ```sh
    kubectl apply -f k8s/orchestrator-rbac.yaml
    ```

3. **Restart the orchestrator deployment to pick up new permissions:**

    ```sh
    kubectl rollout restart deployment orchestrator
    ```

This is required for the orchestrator to successfully create game server pods in Kubernetes.

---

## 7c. Cleaning Up Old Deployments and Pods

- If you change Deployment selectors or labels, you must delete the old Deployments before re-applying:

    ```sh
    kubectl delete deployment redis
    kubectl delete deployment orchestrator
    kubectl delete deployment gatekeeper
    ```

- To remove all pods (if needed):

    ```sh
    kubectl delete pod --all
    ```

- If you see errors about missing apiVersion/kind in a manifest (e.g., game-server.yaml), either fix the file or remove it from the k8s directory.

---

## 7d. Resetting Redis State for Testing

If the orchestrator is not spawning game servers due to stale Redis state (e.g., after a cleanup or crash), reset Redis as follows:

1. Connect to the Redis pod:
   ```sh
   kubectl exec -it deploy/redis -- redis-cli
   ```
2. Run these commands to clear server state:
   ```sh
   DEL servers:standby
   DEL servers:active
   DEL $(redis-cli KEYS 'server:*')
   ```
   Or, to clear all data (if using ephemeral storage):
   ```sh
   FLUSHALL
   ```
3. Restart the orchestrator deployment:
   ```sh
   kubectl rollout restart deployment orchestrator
   ```

KEYS server:*
HGETALL server:<id>

This ensures the orchestrator will spawn a new standby game server pod for a clean test cycle.

---

## 8. Clean Up
- To remove all resources:
  - `kubectl delete -f k8s/`

---

## Example: Minimal Deployment & Service YAML

**Deployment (k8s/example-deployment.yaml):**
```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: example
spec:
  replicas: 1
  selector:
    matchLabels:
      app: example
  template:
    metadata:
      labels:
        app: example
    spec:
      containers:
      - name: example
        image: example:latest
        ports:
        - containerPort: 8080
```

**Service (k8s/example-service.yaml):**
```yaml
apiVersion: v1
kind: Service
metadata:
  name: example
spec:
  type: NodePort
  selector:
    app: example
  ports:
    - protocol: TCP
      port: 8080
      targetPort: 8080
      nodePort: 30080
```

---

## Notes
- Adjust ports, image names, and environment variables as needed.
- For persistent data, define PersistentVolume and PersistentVolumeClaim resources.
- For secrets/configs, use Kubernetes Secrets and ConfigMaps.
- No Helm required; all manifests are plain YAML.

---

**You can now follow this document to migrate your project.**
