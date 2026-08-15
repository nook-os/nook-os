# CI and deploys

One surface: GitHub Actions. Three workflows, all in `.github/workflows/`.

| Workflow      | Trigger           | Produces                                        |
| ------------- | ----------------- | ----------------------------------------------- |
| `ci.yml`      | every push and PR | nothing — it only says yes or no                |
| `release.yml` | a `v*` tag        | `nook` binaries, and images on ghcr.io          |
| `rc.yml`      | dispatched by ref | `rc-<sha>` images on ghcr.io, and nothing else  |

Nothing deploys on a branch push, and nothing builds a release without a tag,
so `main` never produces something that looks shipped but isn't.

## CI

`cargo fmt --check`, `clippy`, `cargo test --workspace`; `pnpm -r typecheck`;
and a drift check that regenerates the TypeScript types from Rust and fails if
the committed ones differ.

The Rust job runs against a real Postgres service container with
`NOOK_REQUIRE_DB=1`. Without a database every test that needs one returns
early, and the suite reports success having executed almost nothing — the
failure mode where a green tick means less each time you add a test.
`NOOK_REQUIRE_DB` turns that silent skip into a failure.

## Releases

```
git tag -a v0.3.0 -m "…"
git push origin v0.3.0
```

That builds:

- **Binaries** — `nook-{linux,darwin}-{x86_64,aarch64}`, each with a `.sha256`
  beside it, attached to the GitHub release.
- **Images** — `ghcr.io/nook-os/nook-{control,node,web}`, tagged with the
  version and `latest`, for `linux/amd64` and `linux/arm64`.

Every binary is built on a runner of its own architecture rather than
cross-compiled, and the images likewise build per-arch and merge into one
manifest. Building under QEMU is the obvious alternative, and it emulates the
whole Rust compile — turning a five-minute build into forty.

Ubuntu 22.04 rather than `latest`: a binary carries the glibc it was built
against, and 24.04's is newer than Debian 12, the distro most people
self-hosting this are running.

## What a build tells the board

`rc.yml` writes an **Images** report on the card the built commit's pull request
closes — a table of image, tag and digest, one row per image it built. It is
keyed `images`, and a report key is an address: building the same pull request
again replaces that table rather than adding a second one, so a card carries the
current answer and not a log.

The report is Nook reporting on itself through the ordinary producer surface
(`PUT /api/v1/tasks/{key}/reports/images`), with nothing special about it — the
same door any other CI system writes through.

To turn it on, a repository needs two settings and neither is optional:

| Setting              | Kind     | Value                                       |
| -------------------- | -------- | ------------------------------------------- |
| `NOOK_URL`           | variable | the control plane's base URL                |
| `NOOK_REPORTS_TOKEN` | secret   | a `reports:write` token for this workspace  |

Mint the token against your own session, narrowed to both the scope and the
workspace:

```
curl -X POST "$NOOK_URL/api/v1/tokens" \
  -H "Authorization: Bearer <your own token>" \
  -H 'content-type: application/json' \
  -d '{"name":"github actions — image reports",
       "scopes":["reports:write"],
       "workspace":"nook-os"}'
```

The value comes back once. That grant writes and retracts reports on cards in
that one workspace and reaches nothing else — not the board, not the notebook,
not another workspace's cards. A personal token would also work and is the wrong
thing to hand a CI job: the point of MAIN-602's scopes is that a credential
sitting in a runner can do the one job it is there for.

With either setting missing — a fork, or a repository nobody has configured —
the step says so and passes. That is the rule for every way this can fail:

- the built commit belongs to no pull request, or its body has no `Closes KEY`
  line, or the key names no card the token can reach;
- the control plane is unreachable, or the token has been revoked.

Each of them is a `::notice::` or `::warning::` in the log and a green job.
Publishing a report is a remark **about** a build, and a remark that cannot be
delivered is not a reason to throw away images that built correctly.
`scripts/publish-image-report.test.sh` runs the real publisher against fakes for
`gh` and `curl` and holds it to that in every one of those cases; it runs in
`./test.sh lint` and in CI.

## Deploying

The quickest path is the installer, which asks these questions and generates
the files:

```
curl -fsSL https://nookos.dev/install.sh | sh
```

### Which modes bring their own Postgres

| Mode | Postgres |
| --- | --- |
| Docker Compose | **Included** — runs as a service, with a generated password |
| Docker Compose behind Traefik | **Included** — same |
| `docker run` | **Bring your own** — you supply `DATABASE_URL` |
| systemd + native binary | **Bring your own** — you supply `DATABASE_URL` |

NookOS does not install or manage Postgres on your host. The two Compose modes
run it as a container alongside everything else; the other two expect one you
already operate, which is usually what you want if you have a managed instance
or an existing cluster.

For a bring-your-own mode, the whole prerequisite is a role and a database:

```sql
CREATE ROLE nook LOGIN PASSWORD 'choose-something';
CREATE DATABASE nook OWNER nook;
```

The schema needs no action — `sqlx::migrate!` runs the migrations at startup.
`nook server init` checks the URL connects before writing anything, so a typo
fails at the prompt rather than as a crash-looping container.

### By hand

A deployment pulls published images; it never builds. The compose file on the
deploy host references tags:

```yaml
services:
  control-plane:
    image: ghcr.io/nook-os/nook-control:v0.2.0
  web:
    image: ghcr.io/nook-os/nook-web:v0.2.0
```

```
docker compose -f docker-compose.prod.yml pull
docker compose -f docker-compose.prod.yml up -d
```

Pin the version rather than `latest`: it makes a rollback a one-line edit
instead of an archaeology exercise, and it means two hosts brought up a week
apart are running the same thing.

Migrations run at startup and are append-only, so bringing a new image up
against an existing database converges it. There is no separate schema step —
and nothing that re-stamps a checksum, because a checksum you rewrite is a
proof that says "verified" without anything having been verified.

**Never** run `docker compose down -v` against a deployment. That is the
bootstrap loop from `CLAUDE.md`, and it destroys real data.

### Which modes can serve tunnels

`nook tunnel` publishes a port at `<label>.<TUNNEL_DOMAIN>`, and a tunnel is a
hostname something has to route — so it works only where a reverse proxy sits in
front of the control plane. Direct Compose, `docker run`, systemd + native
binary and the desktop app have no proxy at all, so tunnels cannot work on them
until you put one there. The two modes that have one write the rule when you
name the zone: Compose behind Traefik generates the wildcard router when `nook
server init` is given a tunnel domain, and the Helm chart renders a `*.<zone>`
ingress rule pointed at the **control-plane** Service when `ingress.tunnelHost`
is set. Both set `TUNNEL_DOMAIN` from that same value, and both leave you the
wildcard DNS record and the certificate.

[`tunnel-proxy.md`](tunnel-proxy.md) is the contract to configure a proxy to —
whole path space to the control plane, `Host` preserved, upgrades forwarded —
with worked nginx, Caddy and Traefik examples, and the DNS and certificate
prerequisites no proxy config can supply.

## The agent port

Nodes do not connect through the reverse proxy that serves the API. TLS for
the agent listener terminates in the control-plane process, because only it
knows which tenant's CA a given client certificate should be judged against —
a proxy that terminated TLS would hold the certificate and hand the control
plane plaintext.

`deploy/enable-agent-mtls.sh` sets that up. It generates the listener's
certificate, prints the fingerprint that goes into join tokens, and adds a
Traefik **TCP passthrough** router so node connections ride the 443 that is
already open and reach port 8081 untouched. Passthrough is the whole point:
the proxy routes on SNI and never opens the stream.

The certificate is self-signed on purpose. Nodes pin its fingerprint, which is
strictly stronger than trusting any public CA that could be persuaded to issue
for the hostname.

## Kubernetes (Helm)

**Start here: `install.sh --k8s`.** The installer recognises a Kubernetes intent
and hands off to Helm rather than putting a binary on disk — it prints the exact
`helm install oci://…` command and writes a starter `nook-values.yaml` in the
current directory, and needs neither `helm` nor `kubectl` present to do so:

```bash
curl -fsSL https://<your-nook>/install.sh | sh -s -- --k8s
# or the no-argument menu → [3] Kubernetes
```

Served from a running control plane, the printed command is pinned to that
control plane's chart version; from the generic domain it omits `--version` (helm
pulls the latest). Fill in `nook-values.yaml` (host, URLs, the Secret name, the
agent block — see the comments) and run the command. It installs nothing on the
local machine; NookOS runs from the chart.

To run the control plane and web front-end on a cluster — against a Postgres you
already operate, with secrets from your own secret manager — use the Helm chart
at [`charts/nook-control/`](../charts/nook-control/README.md). It deploys the
control plane and the nginx `web` image, an HTTP Ingress, external Postgres only,
and secrets by reference (`existingSecret`) — no bundled dependencies, no
migration Job (the control plane migrates at startup, advisory-locked). See the
chart README for a minimal `helm install`.

Every `v*` release **publishes the chart to ghcr as an OCI artifact**, versioned
in lockstep with the images (chart `version` == `appVersion` == the release), so
you can install it without cloning the repo:

```bash
helm install nook oci://ghcr.io/nook-os/charts/nook-control \
  --version X.Y.Z \
  --set existingSecret=nook-control-secrets \
  --set ingress.host=nook.example.com \
  --set config.publicBaseUrl=https://nook.example.com
```

`helm show values oci://ghcr.io/nook-os/charts/nook-control --version X.Y.Z`
prints the tunables, and `helm pull` retrieves the package — both without the
source tree. Because the chart version matches the image tags its defaults
deploy, an install with no `--set …image.tag` runs exactly that release's
control-plane and web images. The publish is gated on the chart's lint/template
checks, so a tag that fails chart validation ships no chart.

The **agent port** (above) has a Kubernetes path too: set `agent.enabled=true`
with `agent.tlsSecret` (a TLS Secret holding the listener cert) and
`agent.publicUrl`, and the chart renders a dedicated **L4 / passthrough
LoadBalancer** on 8081 — the same passthrough requirement as the Traefik router,
because TLS still terminates in the control-plane process. For clusters without
a cloud L4 LB, the chart README documents Gateway API `TLSRoute` (passthrough)
and ingress-nginx TCP passthrough as alternatives, plus how to generate the
listener cert and read its fingerprint. Off by default: with `agent.enabled=false`
the API still serves, nodes just cannot join a cluster-hosted control plane.
