# CLI mTLS via Gateway Termination

## Goals

- Require mTLS for navigator-cli traffic at the Gateway edge.
- Keep in-cluster server and sandbox traffic unchanged (HTTP to the service).
- Generate and store a deployment-time CA and CLI client cert in Kubernetes.
- Preserve compatibility with existing http endpoints for local dev or non-mTLS flows.

## Non-goals

- mTLS between sandboxes and the server.
- Server-side client auth in `navigator-server`.
- CSR-based self-service cert issuance.
- Integration with an external PKI or secrets manager.

## Approach

1. Generate a private CA plus server/client certificates during deployment.
2. Configure the Gateway listener for TLS termination and client cert validation.
3. Extend navigator-cli to load client certs and CA for `https://` endpoints.
4. Document how to retrieve CLI certs and set CLI defaults.

## Design details

- Secrets:
  - `navigator-gateway-tls` (server `tls.crt`/`tls.key` for Gateway termination).
  - `navigator-gateway-client-ca` (`ca.crt` used to validate CLI client certs).
  - `navigator-cli-client` (`tls.crt`/`tls.key`/`ca.crt` for user distribution).
- Server cert SANs: `navigator`, `navigator.<ns>.svc`, `navigator.<ns>.svc.cluster.local`,
  `localhost`, `127.0.0.1`, `host.docker.internal`.
- Helm hook Job generates certs with `openssl` and is idempotent (skip if secrets exist).
- Gateway listener uses TLS termination; client certificate validation is configured via the
  Envoy Gateway policy CRD compatible with v1.5.8.
- Listener port should align with exposed cluster ports (prefer 443 for TLS in local k3s).
- navigator-cli continues to accept `http://` endpoints without TLS settings.

## Implementation steps

1. **Values and defaults**
   - Add `gateway.tls.enabled`, `gateway.tls.secretName`, `gateway.tls.clientCaSecretName`.
   - Set `gateway.listenerPort` default to 443 when TLS is enabled.
2. **PKI generation**
   - Add a Helm hook Job (`pre-install, pre-upgrade`) to create the CA, server cert, and
     CLI client cert, and store them as Secrets.
   - Ensure the Job reuses existing Secrets when present.
3. **Gateway TLS + client auth**
   - Update `deploy/helm/navigator/templates/gateway.yaml` to enable TLS termination with
     `certificateRefs` to the server cert secret.
   - Add an Envoy Gateway policy resource to enforce client cert validation using
     `navigator-gateway-client-ca`.
4. **CLI TLS support**
   - Add CLI flags/env: `NAVIGATOR_TLS_CA`, `NAVIGATOR_TLS_CERT`, `NAVIGATOR_TLS_KEY`.
   - For `https://` endpoints, build a `tonic::transport::Endpoint` with `ClientTlsConfig`
     using the CA + client identity.
   - Update HTTP health checks to use rustls with the same credentials.
5. **Local cluster defaults**
   - Update `deploy/kube/manifests/navigator-helmchart.yaml` values to enable mTLS and
     set the Gateway listener port to 443.
6. **Docs and UX**
   - Document how to extract `navigator-cli-client` to `~/.config/navigator/mtls/`.
   - Provide example `NAVIGATOR_CLUSTER=https://127.0.0.1` usage and env vars.

## Test plan

- `mise run helm:lint` after template changes.
- Unit test: CLI builds TLS config when `https://` and TLS envs are present.
- Integration test: CLI connects with client cert; connection fails without client cert.

## Rollout and compatibility

- Keep mTLS off by default in the Helm chart; enable via values to avoid breaking existing
  deployments.
- CLI behavior remains unchanged for `http://` endpoints.

## Open questions

- Confirm the exact Envoy Gateway policy CRD for client cert validation in v1.5.8
  and its required fields.
- Decide whether to flip the default CLI endpoint to `https://` when running against
  local k3s (currently defaults to `http://127.0.0.1:8080`).
