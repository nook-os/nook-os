# From a bare machine to a working executor (MAIN-647)

Joining is not the same thing as being able to work. A machine can join
successfully, report `online` in `nook get nodes`, and claim nothing at all,
forever — because four independent gates each refuse it in silence.

This is the whole path, in order, with the one step that cannot be automated
named rather than discovered.

**Check yourself at any point with:**

```
nook nodes readiness            # this machine, gate by gate
nook nodes readiness azul       # any node, read from the control plane
```

Every unmet line names the command that fixes it. You do not have to be on the
machine for the second form — the control plane already holds every fact it
needs, which matters most for a node that has gone dark.

---

## 1. Put the binary on the machine

```
curl -fsSL https://nook.example.com/install.sh | sh
```

That is all the installer does, deliberately: verify a binary and get out of the
way. Everything below is the binary's job.

## 2. Install the toolchain it will run

The node runs *your* tooling; it does not ship any. Before joining, install:

- **tmux** — every session is tmux-backed. Without it the node appears online
  and every session fails to open.
- **git** — nothing can be checked out without it.
- **an agent runtime** — `claude`, `codex` or `hermes`. A node with only a
  shell can open a terminal and nothing else.
- **Docker** — only for a node that will run `build` jobs. It is what the
  per-job sandbox is started in.

Nothing here is installed for you and nothing is guessed (NG-5): the readiness
check names what is missing and stops there.

## 3. Join — **with a supervisor**

```
nook setup                                  # interactive: asks everything
nook join --server https://nook.example.com \
          --token nook_join_… \
          --service systemd-user            # the automation path
```

`--service` takes `systemd-user`, `systemd-system`, `launchd`, `supervisord`,
`docker` or `none`, and installs and enables that supervisor without asking.
`nook setup --service …` does the same on an already-joined machine, which is
how you fix a node that was joined without one.

**Do not skip it.** `nook update` replaces the binary and exits, expecting
something to start it again. On an unsupervised machine nothing does, and the
node goes offline permanently the next time the fleet updates. A bare `nook
join` now says so, by name, and still succeeds.

## 4. Sign the runtime in — **this step is manual, and stays manual**

```
claude auth login          # on that machine, as the user the agent runs as
```

A device login is a human action and NookOS never performs one for you (NG-2).
It is named here so it is a step rather than a surprise: an unsigned-in runtime
is a node whose spec and build jobs cannot start.

Confirm with `nook nodes readiness` — the `runtime auth` line carries the
signed-in identity once it lands.

## 5. Declare which loop stages it accepts

**A node opts in, and an empty declaration means it accepts nothing.** That is
the safe default — an upgrade must never enrol a machine in agent work — and it
is the gate most easily mistaken for a healthy idle node.

In the agent's environment (the unit file `--service` just wrote, or the
container's `environment:`):

```
NOOK_LOOP_KINDS=spec,decompose,review,epic-run,build,investigate
NOOK_PORT_RANGE=4200-4299
NOOK_MAX_LOOP_JOBS=2
```

Then restart the agent. `nook get nodes` shows the declaration in the `LOOPS`
column: a node accepting nothing reads `NONE`, distinctly from a node that
simply has nothing queued.

`NOOK_PORT_RANGE` is what sessions and build stacks lease their ports from. A
node without one leases nothing, and a workspace declaring a *required*
listener will not start there.

## 6. Let it fetch its sandbox

Nothing to do. A host node pulls the job-sandbox image matching its own agent
version the first time it needs one, reporting `pulling` in the `SANDBOX`
column meanwhile; queued jobs start when it settles, with nothing restarted.

Two things worth knowing:

- **The sandbox gate is not build-only.** A host node with no image claims *no*
  loop work of any kind — not even a spec pass. "This node just does light
  work" is not an option.
- **`NOOK_SANDBOX_IMAGE` is never pulled for you.** Name your own image and you
  own getting it onto the machine.

A containerised node is exempt: it mounts no Docker socket, cannot run a build,
and has nothing to confine.

## 7. Watch it claim something

```
nook nodes readiness <name>     # every gate ✓
nook get nodes                  # LOOPS, SANDBOX, CAPACITY, DISK, AUTH all read
```

---

## The gates

`✗` is a gate that refuses work **now**: leave it and the node claims nothing.
`⚠` is one that does not — the node works today, and you still want to fix it.

| Gate | | Unmet means | Fixed by |
| --- | --- | --- | --- |
| supervision | ⚠ | works until the next self-update, then goes dark for good | `nook setup --service systemd-user` |
| toolchain | ✗ | no session can run an agent | install tmux / git / a runtime |
| runtime auth | ✗ | jobs start and the agent cannot sign in | `claude auth login` **(manual)** |
| sandbox | ✗ | **every** loop kind is refused | usually nothing — it pulls itself |
| loop kinds | ✗ | the node accepts nothing | `NOOK_LOOP_KINDS=…` + restart |
| port range | ⚠ | a required listener cannot be leased | `NOOK_PORT_RANGE=…` + restart |

Supervision is the one that reads worst and is easiest to postpone: nothing in
the control plane gates placement on it, so an unsupervised node claims and runs
work exactly like any other — right up to the deploy that ends it. That is why
`nook join` warns about it in its own right rather than leaving it to a
checklist somebody may not run.

Supervision, toolchain, runtime auth and loop kinds are all read off the node's
own capability report, which is why `nook nodes readiness <name>` can answer for
a machine you cannot reach.
