# CI and deploys

One surface: GitHub Actions. Two workflows, both in `.github/workflows/`.

| Workflow      | Trigger           | Produces                                        |
| ------------- | ----------------- | ----------------------------------------------- |
| `ci.yml`      | every push and PR | nothing — it only says yes or no                |
| `release.yml` | a `v*` tag        | `nook` binaries, and images on ghcr.io          |

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

## Deploying

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
