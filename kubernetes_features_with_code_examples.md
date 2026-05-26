# Kubernetes Features Illustrated with Project Code Examples

This document provides concrete examples from the current project to illustrate each of the highlighted Kubernetes features, with references to relevant code and configuration files.

---

## 1. Gérer les déploiements et leurs emplacements (Manage deployments and their placement)
- **Example:**
  - The deployment of the `gatekeeper` service is defined in [k8s/gatekeeper/deployment.yaml](k8s/gatekeeper/deployment.yaml#L2-L10), specifying the container image, replicas, and node selectors for placement.
- **Reference:**
  - See lines 2-10 in [k8s/gatekeeper/deployment.yaml](k8s/gatekeeper/deployment.yaml#L2-L10)

---

## 2. Gérer l'équilibrage des charges (Manage load balancing)
- **Example:**
  - The `service-http.yaml` in [k8s/gatekeeper/service-http.yaml](k8s/gatekeeper/service-http.yaml#L2-L10) exposes the `gatekeeper` pods via a Kubernetes Service, which automatically load-balances incoming HTTP traffic across available pods.
- **Reference:**
  - See lines 2-10 in [k8s/gatekeeper/service-http.yaml](k8s/gatekeeper/service-http.yaml#L2-L10)

---

## 3. Gérer la communication entre les conteneurs (Manage inter-container communication)
- **Example:**
  - The `orchestrator` and `game` services communicate via internal Kubernetes networking, as defined by their respective Services and Deployments. For example, see the orchestrator deployment at [k8s/orchestrator/deployment.yaml](k8s/orchestrator/deployment.yaml#L2-L10).
- **Reference:**
  - See lines 2-10 in [k8s/orchestrator/deployment.yaml](k8s/orchestrator/deployment.yaml#L2-L10)

---

## 4. Gérer la découverte de services (Manage service discovery)
- **Example:**
  - Services like `redis` are discoverable by other pods using the service name (e.g., `redis`), as defined in [k8s/redis/service.yaml](k8s/redis/service.yaml#L2-L10).
- **Reference:**
  - See lines 2-10 in [k8s/redis/service.yaml](k8s/redis/service.yaml#L2-L10)

---

## 5. Gérer les mises à jour (Manage updates)
- **Example:**
  - Rolling updates are handled by Kubernetes Deployments, as seen in [k8s/orchestrator/deployment.yaml](k8s/orchestrator/deployment.yaml#L2-L10), which allows updating the container image with zero downtime.
- **Reference:**
  - See lines 2-10 in [k8s/orchestrator/deployment.yaml](k8s/orchestrator/deployment.yaml#L2-L10)

---

## 6. Gérer la montée en échelle (Manage scaling)
- **Example:**
  - The number of replicas for each service can be adjusted in the Deployment YAMLs, e.g., `replicas: 1` at [k8s/gatekeeper/deployment.yaml](k8s/gatekeeper/deployment.yaml#L7).
- **Reference:**
  - See line 7 in [k8s/gatekeeper/deployment.yaml](k8s/gatekeeper/deployment.yaml#L7)

---

## 7. Gérer le stockage nécessaire à la persistance des données (Manage persistent storage)
- **Example:**
  - Redis uses a StatefulSet and PersistentVolumeClaim for data persistence, as defined in [k8s/redis/statefulset.yaml](k8s/redis/statefulset.yaml#L2-L10).
- **Reference:**
  - See lines 2-10 in [k8s/redis/statefulset.yaml](k8s/redis/statefulset.yaml#L2-L10)

---

## 8. Gérer la configuration et les secrets (Manage configuration and secrets)
- **Example:**
  - Environment variables and secrets are injected into pods via the `env` section, e.g., at [k8s/gatekeeper/deployment.yaml](k8s/gatekeeper/deployment.yaml#L28-L35).
- **Reference:**
  - See lines 28-35 in [k8s/gatekeeper/deployment.yaml](k8s/gatekeeper/deployment.yaml#L28-L35)

---

## 9. Gérer les dysfonctionnements (Manage faults)
- **Example:**
  - Liveness and readiness probes in deployment files ensure that unhealthy pods are restarted automatically. See [k8s/gatekeeper/deployment.yaml](k8s/gatekeeper/deployment.yaml#L37-L46).
- **Reference:**
  - See lines 37-46 in [k8s/gatekeeper/deployment.yaml](k8s/gatekeeper/deployment.yaml#L37-L46)

---

## Additional Code References
- **Service Logic:**
  - Game logic: [crates/game/src/main.rs](crates/game/src/main.rs)
  - Gatekeeper logic: [crates/gatekeeper/src/main.rs](crates/gatekeeper/src/main.rs)
  - Orchestrator logic: [crates/orchestrator/src/main.rs](crates/orchestrator/src/main.rs)
- **Dockerfiles:**
  - Container build instructions: [dockerfiles/](dockerfiles/)

---

This mapping demonstrates how each Kubernetes feature is realized in the project, with direct links to the relevant configuration and code files.