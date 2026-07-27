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

## What it is not

- **Unauthenticated.** The CLIs ship without credentials (NG-1); agent
  device-auth surfacing is a follow-up ticket.
- **Not an executor.** No loop-jobs queue or scheduling logic lives here — this
  is pure substrate (NG-2). The `capabilities.shared_operator` designation it
  reports (AC-4) lets later executor selection tell it apart from personal nodes.
