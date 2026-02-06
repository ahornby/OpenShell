# Cluster Bootstrap Plan

## Goals

- Make cluster deployment part of the customer-facing CLI.
- `navigator cluster admin deploy` provisions a local k3s cluster in Docker (k3s-in-container).
- The only external dependency for cluster deployment is Docker.
- Enable programmatic cluster creation from e2e tests and mise via a shared crate.
- Allow limited dev-only overrides for applying extra Helm charts or manifests.

## Non-goals

- No production or remote cluster support.
- No host-installed `kubectl` or `helm` requirements.
- No multi-node or HA configuration.

## Approach

1. Add a new crate: `crates/navigator-bootstrap`
   - Owns cluster lifecycle for local dev (create/start) using Docker only.
   - Runs a custom cluster runtime image based on `rancher/k3s` with required ports and volumes.
   - Writes kubeconfig by reading `k3s.yaml` from the container and rewriting the server address.
   - Bakes baseline Gateway API manifests and an Envoy Gateway HelmChart AddOn into `/var/lib/rancher/k3s/server/manifests` in the custom image.

2. CLI integration
   - Add `navigator cluster admin deploy` subcommand in `crates/navigator-cli`.
   - Wire to `navigator-bootstrap` APIs.
   - Minimal flags: `--name`, `--update-kube-config`, `--get-kubeconfig`.
   - Default behavior matches current `mise -C kube run start` intent without k3d.
   - `--get-kubeconfig` prints the stored kubeconfig for the cluster.
   - `--update-kube-config` writes the stored kubeconfig into the user's local kubeconfig.

3. Mise and e2e usage
   - Update `mise.toml` and `kube/mise.toml` to call `navigator cluster admin deploy`.
   - Ensure `test:e2e:sandbox` uses the same entrypoint for cluster creation.

4. Documentation
   - Update `CONTRIBUTING.md` to point to the CLI flow.
   - Document the dev override file format and guardrails.

## Design details

- Docker container name: `navigator-cluster-{name}`; Docker network: `navigator-cluster` (create if missing).
- Port mappings: `6443:6443` (API), `80:80` and `443:443` (Gateway traffic).
- Storage: Docker volume `navigator-cluster-{name}` for `/var/lib/rancher/k3s`.
- k3s args: include `--disable=traefik` to avoid ingress conflicts; add `--tls-san` for `127.0.0.1,localhost,host.docker.internal`.
- Cluster runtime image: `navigator-cluster-runtime:{version}` built from `rancher/k3s:{version}` with baked AddOn manifests.
- Manifests directory: `/var/lib/rancher/k3s/server/manifests` (auto-deployed as AddOns).
- Kubeconfig storage: store cluster kubeconfig under `XDG_CONFIG_HOME/navigator/clusters/{name}/kubeconfig` (fallback to `~/.config`).
- Local kubeconfig update: only when explicitly requested via `--update-kube-config` or `navigator cluster admin kubeconfig update`.
- Local kubeconfig update: only when explicitly requested via `--update-kube-config`.
- Kubeconfig rewrite: replace `server:` with `https://127.0.0.1:6443`.
- Idempotency: `navigator cluster admin deploy` should be safe to re-run; if the container exists, ensure it is running and refresh kubeconfig.

## Docker integration

- Use the `bollard` crate (Docker Engine API over the local socket).
- Use a configurable cluster runtime image reference; CLI does not build images.
- Mise can build `docker/Dockerfile.cluster`; later we will default to a published image.
- Create network/volume if missing, then create or start the container.

Example flow (summary):

- `ensure_network("navigator-cluster")` + `ensure_volume("navigator-cluster-{name}")`.
- `ensure_image("navigator-cluster-runtime:{version}")` -> `inspect_container` -> start if exists, else `create_container` with port bindings and privileged mode.
- `exec_capture("cat /etc/rancher/k3s/k3s.yaml")` -> rewrite server to `https://127.0.0.1:6443` -> store in XDG config.

## Bootstrap resources

- Gateway API CRDs: bake a pinned version manifest into the image's manifests dir for AddOn auto-deploy.
- Envoy Gateway: bake a HelmChart AddOn manifest into the image's manifests dir.
- Navigator ingress: include a Gateway + HTTPRoute manifest that routes to the Navigator service so the server is reachable after deploy.

## Public API sketch

- `navigator_bootstrap::deploy_cluster(options) -> ClusterHandle`
- `ClusterHandle::kubeconfig_path()`
- `navigator_bootstrap::stored_kubeconfig_path(name) -> PathBuf`
- `navigator_bootstrap::print_kubeconfig(name)`
- `navigator_bootstrap::update_local_kubeconfig(name, target_path)`
- `navigator_bootstrap::ensure_cluster_image(version) -> ImageRef`
- `ClusterHandle::stop()` (optional, if we want a CLI stop command later)

## Milestones

1. Bootstrap crate with Docker create/start + kubeconfig rewrite.
2. Build custom cluster runtime image with baked AddOn manifests.
3. CLI wiring and flags; reuse in mise and e2e tasks.
4. Docs updates and examples.

## Test plan

- Unit tests for kubeconfig rewrite logic.
- Integration test (optional) that asserts container exists and kubeconfig points to `127.0.0.1`.
- E2E sandbox uses `navigator cluster admin deploy`.

## Open Questions

## Proposed defaults

- Keep `navigator cluster admin deploy` scoped to bootstrap only (no Navigator chart install).
- Standardize on `rancher/k3s:v1.29.8-k3s1` (or the latest 1.29 patch release at implementation time).
- Use `navigator-cluster:{version}` built from `rancher/k3s:{version}` with baked Gateway API manifests and Envoy Gateway HelmChart AddOn.
