# nook-control Helm chart

Runs the **NookOS control plane + web front-end** on Kubernetes:

- two Deployments — the control plane and the nginx `web` image (serves the SPA
  and proxies `/api`, `/mcp`, `/healthz`, `.well-known` to the control plane),
- a Service for each, and a single HTTP Ingress fronting `web`,
- **external Postgres only** — no bundled database, no subcharts,
- **secrets by reference** — you supply a pre-existing Kubernetes Secret; the
  chart never stores or creates secret material.

Migrations run in the control plane at startup (advisory-locked, safe with
multiple replicas), so there is no separate migration Job — a `helm upgrade` to
a newer image tag rolls the Deployment and the new image converges the schema.

## Prerequisites

- Kubernetes ≥ 1.23 and Helm 3.
- A reachable **external Postgres**, and its URL in a Secret.
- An Ingress controller (for the default HTTP Ingress).

## Minimal install

1. Create the Secret the chart references (Postgres URL + a 32+ char session
   secret; add `SECRETS_KEY` and any OIDC/S3 secrets for production):

   ```bash
   kubectl create secret generic nook-control-secrets \
     --from-literal=DATABASE_URL='postgres://user:pass@db.example.com:5432/nook' \
     --from-literal=SESSION_SECRET="$(openssl rand -hex 32)"
   ```

2. Install, pointing at that Secret and your host:

   ```bash
   helm install nook charts/nook-control \
     --set existingSecret=nook-control-secrets \
     --set ingress.host=nook.example.com \
     --set config.publicBaseUrl=https://nook.example.com \
     --set config.webOrigin=https://nook.example.com
   ```

The control-plane pod reaches Ready once `/healthz` passes (Postgres reachable);
the web pod serves the SPA and proxies `/api` to the control-plane Service.

## Secrets (by reference)

`values.existingSecret` names a Secret you manage. The chart wires env vars from
it with `secretKeyRef` — nothing secret is ever rendered into a manifest.
`secretKeys` maps env vars to keys inside that Secret:

| Env var | `secretKeys` key | Required | Notes |
|---|---|---|---|
| `DATABASE_URL` | `databaseUrl` | yes | external Postgres |
| `SESSION_SECRET` | `sessionSecret` | yes | 32+ chars |
| `SECRETS_KEY` | `secretsKey` | prod | 64 hex; vault key. Omit → derived from `SESSION_SECRET` (dev only) |
| `OIDC_CLIENT_SECRET` | `oidcClientSecret` | if OIDC | |
| `NOOK_S3_ACCESS_KEY_ID` | `s3AccessKeyId` | if S3 | |
| `NOOK_S3_SECRET_ACCESS_KEY` | `s3SecretAccessKey` | if S3 | |

Set an optional key's value to the key name inside your Secret to wire it; leave
it `""` to omit that env var. Non-secret config (`APP_ENV`, `PUBLIC_BASE_URL`,
`WEB_ORIGIN`, OIDC issuer/client id, S3 bucket/endpoint, …) comes from
`values.config` via a ConfigMap.

## Ingress & TLS

`ingress.className`, `ingress.host`, `ingress.annotations`, and TLS are values.
For TLS either reference an existing Secret:

```yaml
ingress:
  tls: { enabled: true, secretName: nook-tls }
```

or drive cert-manager with annotations and `tls.enabled: true` (no `secretName`):

```yaml
ingress:
  annotations: { cert-manager.io/cluster-issuer: letsencrypt-prod }
  tls: { enabled: true }
```

## Security

Both pods run non-root with dropped capabilities and a `RuntimeDefault` seccomp
profile. The control-plane image runs as uid 10001. The stock-nginx web pod runs
as uid 101 with only `NET_BIND_SERVICE`, a read-only root filesystem, and
emptyDir mounts over nginx's writable paths. A dedicated ServiceAccount is
created by default; `nodeSelector`, `tolerations`, `affinity`, and
`podAnnotations` are all overridable.

## What this chart does NOT do

- Deploy Postgres, Redis, an object store, or any third-party dependency
  (external, by design).
- Create or populate the Secret (you manage it / your secret manager does).
- Expose the agent mTLS listener (`:8081`) — a separate Service issue.
- Publish the chart to a registry, or serve the SPA from the control plane.

## Validate the render

```bash
helm lint charts/nook-control
helm template nook charts/nook-control \
  --set existingSecret=s --set ingress.host=nook.example.com
```

See also [`docs/ci-deploy.md`](../../docs/ci-deploy.md) for the compose/native
deploy paths. Full in-cluster bring-up (kind) is a separate issue; this chart is
validated by `helm lint` + `helm template`.

## Values

Every key in [`values.yaml`](values.yaml) is documented inline.
