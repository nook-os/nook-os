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

## What it is not

- **Unauthenticated.** The CLIs ship without credentials (NG-1); agent
  device-auth surfacing is a follow-up ticket.
- **Not an executor.** No loop-jobs queue or scheduling logic lives here — this
  is pure substrate (NG-2). The `capabilities.shared_operator` designation it
  reports (AC-4) lets later executor selection tell it apart from personal nodes.
