# CI and deploys

`Jenkinsfile` at the repo root builds, tests, and — on the deploy branch only —
ships. It carries no infrastructure details: every environment-specific value
arrives as an environment variable on the Jenkins job, so the same file works
for anyone who clones this repo and nothing about a particular network is
committed here.

## What it does

| Stage    | Runs on            | Notes                                                   |
| -------- | ------------------ | ------------------------------------------------------- |
| Rust     | every branch       | `cargo fmt --check`, `clippy`, `test`                    |
| Frontend | every branch       | `pnpm -r typecheck`                                      |
| Images   | every branch       | builds all three; pushes only when a registry is set     |
| Deploy   | deploy branch only | schema, then `compose pull && up -d`                     |
| Health   | deploy branch only | polls until the control plane answers, or fails          |

A feature branch therefore gets the full build and test treatment and touches
nothing. That's the "push a branch to see if it's good" loop; only the deploy
branch reaches a running system.

`main` is the deploy branch. Keeping CI on a side branch meant every change had
to be landed twice and the two copies drifted — the deployed app and `main`
disagreed about what the product was, which is exactly the confusion this is
supposed to remove.

The agent needs `docker`, `git` and `curl`. Toolchains come from containers
(`rust:1-slim-bookworm`, `node:22-slim`), so there is no agent-side setup to
drift out of sync with the repo.

## Job configuration

| Variable             | Required | Meaning                                          |
| -------------------- | -------- | ------------------------------------------------ |
| `NOOK_REGISTRY`      | to push  | Image prefix, e.g. `registry.example.com/nookos` |
| `NOOK_REGISTRY_CRED` | to push  | Jenkins username/password credential id for it   |
| `NOOK_DEPLOY_DIR`    | to deploy | Checkout on the deploy host that compose runs in |
| `NOOK_COMPOSE_FILE`  | no       | Default `docker-compose.prod.yml`                |
| `NOOK_PG_CONTAINER`  | to deploy | Postgres container name, for the schema step     |
| `NOOK_HEALTH_URL`    | no       | Polled after deploying                           |
| `NOOK_DEPLOY_BRANCH` | no       | Default `main`                                   |
| `NOOK_DEPLOY_IMAGE`  | no       | Run the deploy in this image (see below)         |
| `NOOK_DEPLOY_MOUNTS` | no       | Extra `-v` args for that container               |
| `NOOK_DEPLOY_PREP`   | no       | Prerequisite install for that image              |

Leave `NOOK_DEPLOY_DIR` unset and the pipeline is build-and-test only — which
is what you want for a second Jenkins job watching development branches.

### When the agent can't read the deploy directory

Jenkins usually runs as an unprivileged user, and a deploy directory holding a
production `.env` usually isn't readable by it. Rather than loosen either,
set `NOOK_DEPLOY_IMAGE` and the deploy runs in a container instead: bind
mounts are resolved by the docker daemon, which is root, so a root container
reads a directory the Jenkins user cannot. The directory is mounted at its own
path so compose resolves everything exactly as it would on the host.

That container needs the docker socket (mounted for you), plus whatever the
deploy checkout fetches from — hence `NOOK_DEPLOY_MOUNTS`. If the registry is
private, it also needs credentials to pull with: `docker compose pull` sends
auth from the *client's* config, and a fresh container has none. Mounting the
host's docker config read-only is the least secret-handling way to do it:

```
-v /root/.docker:/root/.docker:ro
``` A minimal image
like `docker:cli` lacks `bash`, `coreutils` and `curl` that `apply-schema.sh`
uses, so `NOOK_DEPLOY_PREP` installs them:

```
NOOK_DEPLOY_IMAGE = docker:cli
NOOK_DEPLOY_PREP  = apk add --no-cache bash coreutils curl
```

## Build once, deploy by pulling

The pipeline builds `nook-control` and `nook-web` straight from
`deploy/docker/*.Dockerfile`, tags each with the commit sha *and* `latest`, and
pushes both. There is no node image: a node runs the user's tooling in the
user's checkouts, so it is joined natively on each machine rather than shipped
as a container. A deploy is then `docker compose pull && docker compose up -d`:
the deploy host compiles nothing, so deploys are fast and every host runs
byte-identical images.

The compose file on the deploy host should therefore reference
`${NOOK_REGISTRY}/nook-control:latest` and friends rather than carrying `build:`
sections. Rolling back is re-tagging a known-good sha as `latest` and pulling
again — no rebuild, no source checkout.

Leave `NOOK_REGISTRY` unset and images are still built (so a branch build
proves the Dockerfiles work) but nothing is pushed.

## Why the deploy step resets rather than pulls

The deploy checkout no longer builds anything, but it still holds the compose
file and the schema deltas, so the pipeline moves it to the built commit rather
than copying files in. It uses `git reset --hard origin/<branch>` rather than
`git pull`: this history gets rewritten from time to time, and a pull would
either fail or, worse, quietly merge.

## Why the schema step comes first

`deploy/apply-schema.sh` applies `deploy/prod-schema-delta.sql` and re-stamps
the sqlx migration checksum. It runs before the new images start because the
new binary may expect columns the old schema lacks, and the deltas are additive
by construction, so the old binary keeps working until it's replaced.

Skipping the re-stamp takes the control plane down on boot: sqlx keeps a
SHA-384 of every applied migration and refuses to start when the file differs —
which, under the single-migration bootstrap workflow in `CLAUDE.md`, it always
does. **Never** run the bootstrap `docker compose down -v` against a deployment;
it destroys real data.

## Triggering on push

Point the job at the repo and let Jenkins poll, or have the git server call the
job's build-trigger URL from a `post-receive` hook. The hook belongs on the git
server, not in this repo.
