# nook-operator-node

The NookOS **shared operator node** (MAIN-125) as an in-cluster StatefulSet: a
full NookOS node with the loop toolchain (`claude`, `hermes`, `codex`,
`copilot`, plus `git`/`tmux`) preinstalled, joined to the control plane
non-interactively at first boot. It gives a deployment one machine that can
execute loop work so a web-only PM does not have to run their own node.

It sits beside the `nook-control` chart and installs independently — the control
plane and its node are deployed separately.

## Prerequisites

- A running NookOS control plane with the agent (mTLS) listener reachable.
- A **join token** provisioned in the UI (or `POST /api/v1/nodes/join-tokens`).
  Join stays operator-driven; the chart never mints or stores the token.

## Install

```bash
# 1. Put the join token in a Secret (the chart references it, never embeds it).
kubectl create secret generic nook-operator-join \
  --from-literal=joinToken=nook_join_XXXX

# 2. Install, pointing at the control plane's agent listener.
helm install operator ./charts/nook-operator-node \
  --set server=agent.nook.example.com:8081 \
  --set existingSecret=nook-operator-join
```

`values.server` and `values.existingSecret` are both required — the chart
refuses to render half a join configuration.

## Runs report the CONTROL PLANE's advertised URL (MAIN-465)

`values.server` is the address THIS NODE dials — in-cluster or in-compose,
typically an internal service name (`control-plane:8080`). A headless run's
`nook` CLI should not inherit that: the run's token is minted by the control
plane, and its transcripts and escalations should name an address a human can
recognize. Set `NOOK_PUBLIC_API_URL` **on the control plane** (e.g.
`https://nook.example.com`) and every run it raises is told to dial and report
that canonical URL instead; unset, runs fall back to the node's own dial
address, which is only readable when nodes join from outside the network.
This is deliberately NOT `NOOK_AGENT_PUBLIC_URL` — that names the mTLS agent
listener, which resolves somewhere different on purpose and is never an HTTP
API.

## The fleet's GitHub token (MAIN-407)

Review jobs read pull requests and post their verdict, so the operator needs a
GitHub credential. It goes in the SAME Secret as the join token, under the key
`secretKeys.ghToken` (default `ghToken`), and reaches the pod as
`NOOK_GH_TOKEN`:

```bash
kubectl create secret generic nook-operator-join \
  --from-literal=joinToken=nook_join_XXXX \
  --from-literal=ghToken=github_pat_XXXX
```

The key is `optional: true`, so an existing Secret without it still starts the
pod. A node with no GitHub reach is a supported state — it simply cannot take
review work, and says so by name at preflight rather than reading every PR as
"nothing to review" and reporting a pass that examined nothing.

The node exports the token into each session as `GH_TOKEN`, the name `gh`
itself looks for, so an agent running there is never handed a credential by
hand.

### Least-privilege scopes

This is a **fleet-wide** credential: everything it can do, every review job on
this node can do. Use a **fine-grained** personal access token, limited to the
repositories the fleet reviews, granting only:

| Permission      | Level          | Why                                          |
|-----------------|----------------|----------------------------------------------|
| Issues          | Read and write | Post the `Loop review of <sha>` comment and the `loop-*` labels — a PR's comments and labels are ISSUE endpoints, which is why this row exists |
| Pull requests   | Read and write | List the open PRs and read the diff          |
| Contents        | Read-only      | Read the tree the PR changes                 |
| Metadata        | Read-only      | Mandatory; GitHub grants it implicitly       |

**A classic PAT needs `repo`** — there is no narrower classic scope that can
comment on a private repository's pull requests. Prefer the fine-grained table
above; `repo` is the broad credential the warning below is about.

**The Issues row is the one people miss, and it fails LATE.** A fine-grained
PAT is read-only by default, so an under-scoped token authenticates, lists PRs,
and lets a run review a whole pull request before dying at `POST
issues/comments` with *"Resource not accessible by personal access token"* —
which the run then retries identically on every backoff. Two failures on prod
(2026-08-08) cost five burned passes before a human read the transcript. Since
MAIN-469 the loop names both: the delivery error says the token lacks
Issues/Pull requests write, and a **dead** token (401) surfaces on the
workspace's review-loop panel as *forge credential rejected* rather than as a
repo with nothing open.

**Epic-run passes need one elevation** (MAIN-144): the epic-runner is the
loop's merge authority, and merging a pull request requires **Contents: Read
and write** on the repositories the runner manages. Grant it only if this
fleet runs `epic-run` jobs; a review-only fleet keeps the read-only table
above. A pass whose merge is refused for a missing permission stops and names
that exact reason in the job transcript and the epic's comment — it never
silently skips the PR.

Nothing else. In particular **not** Administration, **not** Actions, **not**
Workflows, and **not** a classic token's `repo` scope — `repo` carries push and
settings access to every repository the issuing account can reach, which is
precisely the fleet-wide credential nobody audits later. If a classic token is
unavoidable, `public_repo` is the nearest equivalent and works only for public
repositories.

The token is never written to a chart value or a committed file; it exists only
in the Secret, and `scripts/check-secrets-untracked.test.sh` is what keeps that
true.

### Per-workspace tokens outrank this one (MAIN-456)

A workspace can hold its **own** forge token — Workspaces → *forge token* in the
UI, or `PUT /api/v1/workspaces/{id}/gh-token` — and where one is set it is used
instead of `NOOK_GH_TOKEN` for everything above: the demand poll, the verdict
comment and labels, and the `GH_TOKEN` a run's `gh` sees. It is the multi-tenant
answer: one fleet token would post every tenant's verdicts as one identity.

**The requirement is the same table**, scoped to that one repository: Issues
write, Pull requests write, Contents read, Metadata read (classic: `repo`). The
control plane **exercises a pasted token before sealing it** — it reads the
workspace's repository and probes the two writes with bodies GitHub is certain
to reject, so nothing is created — and refuses the paste with a message naming
the permission that is missing. A token that cannot post a verdict never
reaches the vault.

## Persistence (AC-5)

Three `volumeClaimTemplates` give the pod stable PersistentVolumeClaims it
re-attaches on restart:

| Volume      | Mount                 | Holds                                   |
|-------------|-----------------------|-----------------------------------------|
| `config`    | `/root/.config/nook`  | node identity (`node.toml`) + SSH key   |
| `workspace` | `/workspace`          | cloned repos / worktrees                |
| `tmux`      | `/var/lib/nook/tmux`  | tmux runtime dir (`TMUX_TMPDIR`)        |

A restart re-attaches the same volumes: the node comes back with the **same
identity** and its **checkouts intact**. Deleting the PVCs (a fresh volume) makes
it re-join as a brand-new node rather than half-reusing state.

> Note: a pod restart terminates the tmux **server process**, so in-flight
> interactive sessions do not resume — the volume preserves the socket dir and
> on-disk state, not running processes.

## Network confinement (MAIN-141)

The node runs agent workloads with a full toolchain, so **what it can reach is
the security boundary**. The chart ships a NetworkPolicy, on by default.

### It never needs inbound

Traffic is one-way by design: a node **dials** the control plane and holds that
outbound connection open, and the control plane answers on it. The control
plane never dials a node. So the policy denies ingress completely — expressed
as `policyTypes: [Ingress]` with no `ingress:` rules at all — and that costs
nothing, because nothing legitimate ever arrives.

### Egress: DNS, your control plane, and the public internet

Everything else is denied, including every private range:

| Allowed | Why |
|---|---|
| cluster DNS (UDP+TCP 53) | without it the node resolves nothing |
| the public internet | agents clone from forges and install packages |
| your control plane | see the two patterns below |

`networkPolicy.deniedCIDRs` is subtracted from the internet allowance and
defaults to `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16`,
`100.64.0.0/10`. That last pair matters as much as the RFC1918 three: link-local
is where the **cloud metadata endpoint** (`169.254.169.254`) lives, and some
clusters allocate pod IPs from the carrier-grade NAT range.

> **Enforcement needs a NetworkPolicy-capable CNI** — Calico, Cilium, Antrea and
> friends. Under a CNI that ignores policy (plain kubenet, some managed
> defaults) the object still applies cleanly and restricts **nothing**, with no
> warning from Kubernetes. Check your CNI before treating this as a control.

### Reaching your control plane

**In-cluster** — select it by pod label, so a reschedule cannot stale the rule:

```yaml
networkPolicy:
  controlPlane:
    enabled: true
    namespace: nook
    podSelector:
      app.kubernetes.io/name: nook-control
    port: 8081          # the agent listener, not the browser API
```

**Outside the cluster on a public address** — nothing to do; the internet rule
already covers it.

**Outside the cluster on a private address** — the deny list would sever the
node from it, so punch a hole back through, narrowly:

```yaml
networkPolicy:
  additionalAllowedCIDRs:
    - 10.20.0.5/32      # the control plane, and nothing else in 10/8
```

If your DNS does not live in a namespace labelled
`kubernetes.io/metadata.name: kube-system`, set
`networkPolicy.dns.namespaceLabels` to whatever does.

Turn the whole thing off with `--set networkPolicy.enabled=false`.

## Running each job as a Pod (MAIN-623)

By default this chart runs every loop job as a **process inside the agent's own
pod** (`executor.mode: local`). That is simple and it is what has always
happened, but the jobs share one filesystem, one process table and one crash.

`executor.mode: kubernetes` gives each job a **Pod of its own**:

```bash
helm install operator ./charts/nook-operator-node \
  --set server=agent.nook.example.com:8081 \
  --set existingSecret=nook-operator-join \
  --set executor.mode=kubernetes \
  --set executor.namespace=nook-jobs \
  --set executor.image=ghcr.io/nook-os/nook-job-sandbox:<tag>
```

`executor.namespace` must already exist, and is usually **not** this chart's own
namespace: a job Pod is the untrusted half, and a blast radius that excludes the
agent's pod is worth a namespace of its own.

`executor.image` has no default on purpose. A default would be this chart
guessing at a registry, and a wrong guess fails at `ImagePullBackOff` — long
after the install reported success.

**The agent image has to be built with the `kubernetes` feature.** The Pod
executor is an optional feature of `nook-node`, so that the desktop bundle does
not ship a Kubernetes client to laptops with no cluster. The published
`ghcr.io/nook-os/nook-operator-node` is built with it (see
`deploy/docker/operator-node.Dockerfile`), so a stock install needs nothing
here. A **hand-built** image does: without `--build-arg
NOOK_NODE_FEATURES=kubernetes` you get the ServiceAccount, the Role and
`NOOK_EXECUTOR=kubernetes` in the pod, and a binary that ignores all three — the
mode silently does nothing and every job runs locally.

### What a job Pod gets, and what it does not

- **Its own checkout, from a fresh clone.** A job Pod mounts nothing, so the
  node's clone cache and its per-job worktree are unreachable from inside one;
  the Pod clones the workspace itself at `/home/agent/workspace`. There is no
  PVC and no warm state carried between passes, so a repair pass on a card
  starts cold — a review or build's warm agent session is a host-node feature
  and does not apply here.
- **The same environment contract a container-confined job gets** — `HOME`,
  `CLAUDE_CONFIG_DIR`, `NOOK_SANDBOX=1`, `NOOK_SERVER`, `NOOK_JOB_ID` and the
  job's own variables. One function builds it for both, so an agent cannot tell
  which executor started it — **except for credentials**, which are held back
  here and come from a Secret or not at all (see below).
- **No leased ports.** A cluster job's port would need a Service to be
  reachable and this chart creates none, so the lease is not taken rather than
  delivered and quietly useless.
- **No Docker socket and no privilege**, for every kind this executor runs.

### Exactly what it grants

A **Role**, bound in `executor.namespace` alone. Never a ClusterRole: an
executor reaches the one namespace it creates jobs in, and a cluster-wide grant
would let a compromised agent read every Secret on the cluster — the blast
radius this design exists to avoid.

| resource | verbs |
| --- | --- |
| `pods` | `create`, `get`, `list`, `watch`, `delete` |
| `pods/log` | `get` |

Two rules, and the list is closed. It matches the client's operations exactly,
because a convenience method nobody calls is a permission somebody has to
justify. `ci/validate.sh` asserts the count, so a third rule fails the build.

**`pods/attach` is deliberately absent, and it has a consequence.** Anything
holding it can write into a running container. Without it a job Pod's output
streams to the card exactly as a host node's does, but a **steering message
cannot reach a running Pod job** (MAIN-231). `nook interactions ask --wait` is
unaffected — it goes over the control-plane API, not the agent's stdin — so a
spec job can still raise a question and block on it; it just cannot receive an
unsolicited one. A steering message for a Pod job is refused rather than
silently dropped.

**Left at `local`, this chart renders byte-for-byte what it rendered before**:
no ServiceAccount, no Role, no RoleBinding, no new environment. An upgrade
cannot be the thing that quietly grants a cluster permission.

### Builds need a pool of their own — and are not run here yet

`build` is **not** in `loopKinds` and this executor does not offer it, so
everything in this subsection is inert today: it is the ground MAIN-655 builds
on, and configuring a pool now changes nothing. The one thing it does change is
the refusal you get if a build ever is placed here — with no pool it is
*refused* rather than mis-scheduled.

A build Pod runs a nested Docker daemon and is **privileged**, so it must never
share a node with anything else:

```bash
  --set executor.buildPool.selector=nook.io/pool=build \
  --set executor.buildPool.taint=nook.io/build-only
```

**Both or neither.** Either half alone reads as protection and is not: a
selector with no taint puts builds on a pool nothing keeps other work off, and a
taint with no selector lets a build schedule anywhere. Setting one refuses the
install by name. Declaring neither is fine and means this cluster runs no
builds — a build job is then *refused* rather than mis-scheduled, which leaves
the card's strike budget alone.

Even on its own pool, a privileged build Pod can reach the node it runs on
(MAIN-612). The mitigation is blast radius — a dedicated, disposable pool — and
**not** confinement. Do not describe it as a boundary against a hostile agent.

### Credentials are not a shipped path yet

**By default a job Pod gets no credentials at all, and its agent will fail to
authenticate.** That is the honest state of this mode, not an oversight.

The credentials a job would want — the fleet's GitHub token, the run's own
`NOOK_TOKEN`, the workspace's secret items — are **deliberately not written into
the Pod's environment**. A Pod's `env` values are returned by `get pods` and
printed by `kubectl describe pod`, and the Role above grants `pods
get/list/watch` across `executor.namespace`; putting a token there would publish
it to every principal with pod-read, a far lower bar than `get secrets`, and
store it in etcd under a resource that is not encrypted at rest by default. The
run records on its transcript which credentials it held back.

What exists instead is a **seam**: create a Secret by hand and name it.

```bash
kubectl create secret generic nook-job-credentials -n nook-jobs \
  --from-literal=GH_TOKEN=... --from-literal=NOOK_TOKEN=...
helm upgrade ... --set executor.credentialsSecret=nook-job-credentials
```

Every key in it becomes an environment variable in the job Pod. Nothing in this
chart or in the agent creates, reads or updates that Secret — the executor's
Role grants no `secrets` verb at all, so the agent could not if it tried.

**This is test scaffolding, not a supported way to run in production.** A Secret
read as environment variables is better than a Pod spec but is still not a
credential store: it is namespace-wide, static, and unaudited. MAIN-337/339 own
the real path; until they land, treat cluster-executed jobs as a capability you
are trying out rather than one you depend on.

A `credentialsSecret` naming a Secret that does not exist keeps the Pod in
`CreateContainerConfigError`, and the agent refuses the job naming it — rather
than starting, silently having no credentials, and spending a pass finding out.

## What it is not

- **Unauthenticated.** The CLIs ship without credentials (NG-1); agent
  device-auth surfacing is a follow-up ticket.
- **Not an executor.** No loop-jobs queue or scheduling logic lives here — this
  is pure substrate (NG-2). The `capabilities.shared_operator` designation it
  reports (AC-4) lets later executor selection tell it apart from personal nodes.
