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
| `NOOK_GIPHY_KEY` | `giphyKey` | no | chat's GIF picker. Omit → no GIF button, everything else unchanged |

Set an optional key's value to the key name inside your Secret to wire it; leave
it `""` to omit that env var. Non-secret config (`APP_ENV`, `PUBLIC_BASE_URL`,
`WEB_ORIGIN`, OIDC issuer/client id, S3 bucket/endpoint, …) comes from
`values.config` via a ConfigMap.

`SECRETS_KEY` is a deployment credential that unlocks NookOS's **own** at-rest
secret encryption — a separate concept from these deployment credentials; see
[`docs/secrets-encryption.md`](../../docs/secrets-encryption.md).

**The forge credential is not here.** A GitHub token is set per workspace, in
the UI (Workspaces → *forge token*), and sealed with the vault `SECRETS_KEY`
unlocks — so it is never a chart value. What it must be able to do, and what
happens to a token that cannot, is the
[operator-node chart's forge-credential section](../nook-operator-node/README.md#the-fleets-github-token-main-407).

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

## Tunnels: the wildcard host (`ingress.tunnelHost`)

`nook tunnel 3000` publishes a port in the fleet at `<label>.<zone>`, and that
hostname has to be routed to the control plane by whatever is in front of it.
Set the zone and the chart does both halves — the Ingress rule and the control
plane's own `TUNNEL_DOMAIN`, from one value, so the router and the surface it
routes to cannot disagree:

```yaml
ingress:
  host: nook.example.com
  tunnelHost: tunnels.example.com   # the ZONE, no leading "*."
  tls: { enabled: true, secretName: nook-tls }
```

That renders a **second** rule, `*.tunnels.example.com`, whose `/` points at the
**control-plane** Service — not `web`. Every path on a tunnel host belongs to the
control plane: `host_dispatch` decides what the host means before routing, and
`web`'s SPA fallback would answer every path `200` with the NookOS app shell
instead, with nothing logging an error
([`docs/tunnel-proxy.md`](../../docs/tunnel-proxy.md)). The apex rule is
untouched — `ingress.host` still sends `/` to `web`.

Leave `tunnelHost` empty and nothing changes: one rule, one TLS host, no
`TUNNEL_DOMAIN`, and the tunnel surface stays off.

**The wildcard certificate is yours to arrange.** The chart adds
`*.<tunnelHost>` to `tls.hosts` and creates **no** cert-manager `Certificate` —
issuers, solvers and DNS credentials are cluster policy. Two supported shapes:

- **cert-manager with a DNS-01 solver.** A wildcard is only issuable over
  DNS-01 (HTTP-01 cannot prove a name that does not resolve yet), so the
  ClusterIssuer this chart's `cert-manager.io/cluster-issuer` annotation names
  must have a DNS-01 solver — and it must cover **both** names, because they
  are one certificate (below). Set `ingress.tls.secretName` as well: the chart
  renders `secretName` only when you give it one, and cert-manager's
  ingress-shim issues nothing for a `tls` entry that has none. Nothing resolves
  `*.<tunnelHost>` during issuance, so you can hold the certificate before
  pointing the wildcard record anywhere.
- **A pre-provisioned Secret.** Create the TLS Secret yourself — covering both
  names — and put it in `ingress.tls.secretName`; the chart references it and
  issues nothing.

**The apex and the wildcard share one `tls` entry, so they share one
certificate.** That is what "joins `tls.hosts`" means, and it has two
consequences worth knowing before you set the value. A DNS-01 solver scoped by
`dnsZones` to the tunnel zone alone cannot answer for the apex name in the same
order, and the issuance fails for **both**. And on an existing install whose
apex certificate is issued over HTTP-01, adding `tunnelHost` makes that
certificate un-renewable — a wildcard cannot be proven that way. Move the whole
order to a DNS-01 issuer whose solver covers the apex and the tunnel zone, or
give the zone its own front end.

**A wildcard is one label deep.** `*.example.com` covers `a.example.com` and
does **not** cover `a.b.example.com`, so `tunnelHost` must be exactly the zone
tunnels are served under. Tunnel labels carry no dots, so one wildcard at that
depth covers every tunnel. The chart refuses a `tunnelHost` stored *as* a
wildcard (`*.tunnels.example.com`), and refuses one that is a **parent** of
`ingress.host` — with `tunnelHost: example.com` under an apex of
`nook.example.com` the control plane would read your own apex as a tunnel host
and answer the whole application with its "No such tunnel" page.

You still owe it a **wildcard DNS record** for `*.<tunnelHost>` pointing at the
ingress — labels are minted at runtime, so there is no list of names to create
records for.

**Ingress controllers differ on wildcard hosts.** This was tested on
**ingress-nginx** (`registry.k8s.io/ingress-nginx/controller:v1.11.3`, kind
provider), where the wildcard rule routes `/` and deep paths on a tunnel host to
the control-plane backend while the apex keeps going to `web`. ingress-nginx and
Traefik's Ingress provider both support a wildcard `host:`; some controllers —
notably older AWS ALB and GCE ingress builds — treat `host` as an exact match
and silently route nothing. If yours is one of those, keep `tunnelHost` empty
here and add the wildcard router in that controller's own configuration.

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

## Queue worker & autoscaling (MAIN-153)

The **worker** drains the durable work queue (email sends today; more later). It
is off by default and additive — the chart renders exactly as before until you
enable it:

```bash
helm upgrade --install nook . \
  --set worker.enabled=true \
  --set queue.provider=database \
  --set worker.replicas=2
```

`queue.provider` (`database` | `redis` | `sqs`) is read by the control plane and
the worker alike (`NOOK_QUEUE_PROVIDER`). **`database` is the only provider the
binary runs today**; `redis` and `sqs` are reserved names — the chart renders
their env and KEDA triggers ahead of the implementation, but a worker started on
one refuses to boot until that provider ships. Provider connection material
comes from the `existingSecret` (`secretKeys.redisUrl`, `secretKeys.awsAccessKeyId`
/ `awsSecretAccessKey`); SQS can instead use pod identity
(`queue.sqs.credentialsMode=irsa`, the default). Scope the worker to specific
work types with `worker.workTypes` (`NOOK_WORK_TYPES`).

### KEDA autoscaling (optional)

Set `autoscaling.keda.enabled=true` to scale the worker on queue depth. This
renders a `ScaledObject` (and a `TriggerAuthentication`) whose trigger matches
the provider — a PostgreSQL row count on `work_queue`, a Redis list length, or
SQS queue depth — and KEDA then owns the replica count (`worker.replicas` is
ignored).

> **KEDA must already be installed in the cluster** — this chart does **not**
> install it (see <https://keda.sh/docs/latest/deploy/>). Without KEDA the
> `keda.sh/v1alpha1` objects have no controller and do nothing.

The `ci/validate.sh` render matrix covers all three providers × KEDA on/off.

## Uploads (`config.userContentDir`)

What people upload — ticket and comment attachments — is written to
`config.userContentDir`, backed by a PersistentVolumeClaim the chart creates and
keeps (`helm.sh/resource-policy: keep`, so `helm uninstall` does not take a
tenant's attachments with it). This is **not** the image's
`/usr/local/share/nook/dist`, which holds the release binaries baked in at build
time and is read-only to the uid 10001 the pod runs as.

```bash
# Ephemeral: uploads live only as long as the pod. Nothing else changes.
--set userContent.persistence.enabled=false

# Bring your own claim.
--set userContent.persistence.existingClaim=my-nook-uploads
```

The default claim is `ReadWriteOnce`, which suits `controlPlane.replicas: 1`.
More than one replica needs either a `ReadWriteMany` storage class or
`config.artifactStore: s3` — with S3 the bytes go to the bucket and the volume
is unused.

**A store the control plane cannot write does not stop the pod.** It boots,
serves everything else, and logs one `WARN` naming the backend, the path or
bucket, and the underlying error; uploads answer `503 file storage is not
configured` until it is fixed. `kubectl logs` is where the detail is — the
response body deliberately carries none of it.

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
