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

**The contract:** the chart consumes exactly one Kubernetes Secret, by name
(`values.existingSecret`). How that Secret is populated is your choice of
tooling — **NookOS integrates with no secret manager directly** (no Vault/GCP/AWS
SDK in the control plane, by design); it only ever reads env vars from a
Kubernetes Secret. Keep credentials in your backend, sync them into a Secret,
point the chart at it.

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

`SECRETS_KEY` is a deployment credential that unlocks NookOS's **own** at-rest
secret encryption — a separate concept from these deployment credentials; see
[`docs/secrets-encryption.md`](../../docs/secrets-encryption.md).

### Syncing from a secret manager

Clients keep credentials in Vault, Google Secret Manager, or AWS Secrets
Manager. The pattern is the same for all: a tool (typically the External Secrets
Operator) reads your backend and writes the Kubernetes Secret above; the install
command is unchanged. Worked, copy-adjust examples for each backend — producing
the identical Secret so only the source differs — plus the Secrets Store CSI
driver and Vault Agent Injector alternatives, are in
[`examples/secrets/`](examples/secrets/README.md).

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

## Agent mTLS listener (`:8081`, opt-in)

Nodes join the control plane over a **mutual-TLS** listener on `:8081`. Its TLS
terminates **inside the control-plane process** — the process routes on SNI and
judges each client certificate against the right tenant's CA, so anything in
front must be **L4 / passthrough**: it may route the TCP stream but must never
terminate TLS. A proxy that terminated it would hold the certificate and hand
the control plane plaintext, defeating the pinned-fingerprint design.

It is **off by default**. Turn it on and the chart renders a dedicated
LoadBalancer Service on 8081, mounts the listener certificate, and advertises
the reachable address in join tokens:

```yaml
agent:
  enabled: true
  publicUrl: agent.nook.example.com:8081   # what nodes dial; baked into join tokens
  tlsSecret: nook-agent-tls                 # a TLS Secret holding the listener cert+key
  service:
    type: LoadBalancer
    annotations: {}                         # cloud L4/NLB annotations if needed
```

`agent.enabled=true` **requires** both `agent.tlsSecret` and `agent.publicUrl` —
the chart refuses to render a half-configured listener (a cert-less listener
cannot start; an unadvertised one cannot be dialled). With `agent.enabled=false`
no agent Service is rendered and the control plane still serves the HTTP API.

### Generate the listener cert and pin it (AC-5)

The cert is **self-signed on purpose** — nodes pin its fingerprint, which is
stronger than trusting any public CA that could be persuaded to issue for the
hostname. cert-manager is *not* used for this cert (it is for the public HTTPS
Ingress). Create it once and load it as a TLS Secret:

```bash
# 1. Self-signed listener cert for the advertised name (10y: re-pinning is the
#    rotation cost, and it is the client certs — not this — that authenticate).
openssl req -x509 -newkey rsa:4096 -sha256 -days 3650 -nodes \
  -keyout agent.key -out agent.crt \
  -subj "/CN=agent.nook.example.com" \
  -addext "subjectAltName=DNS:agent.nook.example.com"

# 2. Load it as the TLS Secret the chart references (keys tls.crt / tls.key).
kubectl create secret tls nook-agent-tls --cert=agent.crt --key=agent.key

# 3. The fingerprint each node pins (also printed in NOTES after install):
openssl x509 -in agent.crt -outform der | sha256sum | cut -d' ' -f1
```

Then on each external node, with a join token from the UI:

```bash
nook enroll --server https://agent.nook.example.com:8081 \
  --token <join-token> --server-fingerprint <fingerprint>
```

### Clusters without a cloud L4 LoadBalancer

The default `type: LoadBalancer` assumes a cloud L4 LB. Where that is not
available, expose 8081 by **passthrough** another way (both are documented
options, not the chart default — set `agent.service.type: ClusterIP` and route
to it):

- **Gateway API `TLSRoute` (passthrough mode)** — a `Gateway` listener with
  `tls.mode: Passthrough` and a `TLSRoute` whose `hostname` is the agent SNI
  name, `backendRef` the agent Service. The gateway routes on SNI and never
  decrypts.
- **ingress-nginx TCP passthrough** — expose the stream via the controller's
  `tcp-services` ConfigMap (`8081: "<ns>/<release>-nook-control-agent:8081"`)
  so nginx forwards raw TCP. (This is L4 TCP forwarding, distinct from the
  HTTP Ingress the chart renders for the API/UI.)

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
- Publish the chart to a registry, or serve the SPA from the control plane.
- Run nodes inside the cluster — the agent listener below exposes `:8081` so
  **external** nodes can join; in-cluster node pods are a separate epic.

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
