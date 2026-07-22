---
name: nookos
description: "Run and drive coding agents on OTHER machines with `nook` — no ssh, no tmux. Start a Claude/Codex/bash session anywhere in the fleet, type into it, read its answer."
version: 1.1.0
author: NookOS
license: MIT
platforms: [linux, macos]
metadata:
  hermes:
    tags: [Distributed, Remote-Execution, Claude, Coding-Agent, Fleet, NookOS, Cross-Machine]
    category: autonomous-ai-agents
    related_skills: [claude-code, codex, hermes-agent]
---

# NookOS — running agents on other machines

You are on one machine. NookOS knows about all of them. `nook` is a plain CLI
that talks to the NookOS control plane, so **you never need ssh, a hostname, a
key, or tmux** to do work somewhere else. You name a workspace and a runtime;
the control plane finds a machine that has that repo checked out and starts the
session there.

The single idea worth remembering: **a remote Claude is just a shell you can
type into.** `nook send` types. `nook read` looks. `nook exec` does both and
waits. That's the whole protocol.

---

## 0. Check who you are first

```bash
nook whoami
```

```
server:  https://nook.example.com
as:      you@example.com (user token — can drive any node)
tenant:  hein
```

There are two credentials and only one drives other machines:

| Credential | What it is | Can it drive other machines? |
|---|---|---|
| **user token** (`nook_user_…`) | you, as a person | **yes** — the whole fleet |
| **node token** | this machine's own identity | no — confined to itself |

The confinement is deliberate: a node token sits in a file on a box that runs
other people's code, so one compromised machine must not become every machine.

If `whoami` says *node token*, get a user token from the NookOS UI →
**Settings → Access tokens → new token**, then:

```bash
nook login --token nook_user_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
```

```
✓ logged in to https://nook.example.com as you@example.com
  This CLI can now drive any machine in your fleet.
```

Stored `0600` at `~/.config/nook/auth.toml`. `nook logout` forgets it.

**Install / update:** `curl -fsSL https://<your-nook-host>/install.sh | sh`, and
`nook update` if `nook --help` is missing `start`/`send`/`read`/`exec`.

---

## 1. Look at the fleet

```bash
nook get nodes
```

```
NAME     PLATFORM  STATUS  LAST_SEEN_AT
azul     linux     online  2026-07-21T15:18:55.219722Z
crimson  linux     online  2026-07-21T15:18:53.278669Z
```

```bash
nook get workspaces      # repos NookOS knows about
nook get sessions        # what's already running, and in which runtime
```

```
NAME                       RUNTIME  STATUS    CREATED_AT
Nook@OS: Feedback Session  claude   running   2026-07-21T10:27:06.630679Z
deploy check               claude   exited    2026-07-21T01:40:14.616156Z
```

**Not every machine has every runtime.** `nook get nodes --json` shows each
node's `capabilities.runtimes` — e.g. `azul` has `["claude","codex","bash"]`
while `crimson` has `["hermes","bash"]`. Ask a node for a runtime it doesn't
have and the session fails immediately rather than mysteriously.

---

## 2. Start a session anywhere

```bash
# Claude Code on whichever online machine has this repo:
nook start Nook@OS --runtime claude

# ...or pin the machine and name the session:
nook start Nook@OS --node azul --runtime claude --name refactor-auth
```

```
✓ refactor-auth — claude on azul
  nook send refactor-auth 'your prompt'
  nook read refactor-auth
```

`--runtime` is any executable the node reports: `claude`, `codex`, `hermes`,
`bash`, `zsh`. Sessions are **persistent** (tmux-backed on the node): they
survive your process exiting, your machine rebooting, and the network dropping.
Come back hours later and `nook read` still works.

---

## 3. Talk to a remote Claude — a real transcript

This is the whole point of the skill, so here it is end to end. Every block
below is real output, run from `crimson`, driving Claude on `azul`.

**Start it, then look before you type.** A Claude TUI takes ~10–15s to boot;
a prompt sent into a splash screen is lost.

```bash
nook start Nook@OS --node azul --runtime claude --name fleet-proof
sleep 15
nook read fleet-proof
```

```
── fleet-proof · runtime=claude · status=running ──
▐▛███▜▌   Claude Code v2.1.216
▝▜█████▛▘  Opus 4.8 (1M context) · Claude Max
  ▘▘ ▝▝    ~/.nook/workspace/nook-os/nook-os

 ⚠ 3 MCP servers need authentication · run /mcp

────────────────────────────────────────────────────────────────
❯ Try "create a util logging.py that..."
────────────────────────────────────────────────────────────────
```

**Read the header.** `runtime=claude` means you're looking at a Claude Code TUI
and plain English is the right thing to type. `runtime=bash` means it's a shell
and you should send shell commands. Same command, very different other end.

**Ask it something and wait for the answer:**

```bash
nook exec fleet-proof 'In one short sentence: what machine are you running on and what is this repo?'
```

```
❯ In one short sentence: what machine are you running on and what is this repo?
● You're on a WSL2 Linux box (/home/ryan/.nook/workspace/nook-os/nook-os), and
  this repo is NookOS — a Rust + React system for managing git workspaces,
  worktrees, and tmux-backed terminal sessions across multiple nodes.
✻ Sautéed for 3s
```

**Keep the conversation going** — it's one persistent session, so context
carries between calls:

```bash
nook exec fleet-proof 'add retries to the HTTP client, then run the tests' --timeout 600
nook exec fleet-proof 'commit that with a sensible message'
```

**Clean up when the work is done:**

```bash
nook delete sessions fleet-proof
```

```
✓ Deleted session 'fleet-proof'
```

---

## 4. `exec` vs `send` + `read`

```bash
nook exec <session> 'prompt' --timeout 300     # send, wait for quiet, print
nook send <session> 'prompt'                   # type and return immediately
nook read <session> --lines 400                # screen + scrollback
```

`exec` polls until the screen **stops changing** for two consecutive reads,
which is the only honest way to know an agent has finished — thinking time is
unpredictable, so a fixed `sleep` either truncates the answer or wastes minutes.
`--timeout` is the give-up point, not a fixed wait. Use `--timeout 600` or more
for real coding tasks.

Use `send` + `read` instead when you want to fire a long task and check back
later, or when you're answering a prompt rather than asking a question.

**Submitting is handled for you.** Enter is sent as its own keystroke a beat
after the text, because a TUI treats text-plus-newline arriving in one chunk as
a *paste* and leaves it sitting in the input box unsent. `--no-enter` types
without submitting when you want to stage input.

---

## 5. Menus, permission prompts and interrupts

A Claude permission prompt is just text on screen. Look, then answer:

```bash
nook read fleet-proof            # see the choices, e.g. "1. Yes  2. No"
nook send fleet-proof '1'        # pick one
```

```bash
nook send fleet-proof 'yes'      # free-text confirmations
nook send fleet-proof '/clear'   # slash commands work like anything else
```

For anything the runtime treats as a raw key rather than text (Esc, Ctrl-C),
prefer restarting the session — `nook delete sessions <name>` then `nook start`
— rather than trying to encode control characters.

---

## 6. Driving several machines at once

Sessions are independent, so fan out and collect:

```bash
nook start api      --node azul    --runtime claude --name api-work
nook start frontend --node crimson --runtime claude --name fe-work

nook send api-work 'upgrade the sqlx version and fix the fallout'
nook send fe-work   'migrate the settings page to the new form component'

# ...later
nook read api-work --lines 200
nook read fe-work  --lines 200
```

---

## 7. When to use this instead of running locally

| Situation | Do this |
|---|---|
| The repo is checked out on another machine | `nook start <ws> --runtime claude` |
| This machine lacks `claude` but another has it | `nook start <ws> --node <that one> --runtime claude` |
| Work should outlive your current process | `nook start` — sessions are persistent |
| Several repos need work in parallel | one `nook start` each, then `nook exec` each |
| The task is local and quick | just run it here; don't add a network hop |

---

## 8. Errors you will actually hit

All of these are real messages, not paraphrases.

```
Error: a node token can only act on its own machine — sign in as a user to drive another node
```
You're authenticated as the machine, not as a person. `nook login --token nook_user_…`.
**This is the most likely reason a remote `start`/`send`/`read` fails.**

```
Error: 'crimson' has no online checkout of this workspace
```
That machine doesn't have this repo (or is offline). Drop `--node` to let the
control plane choose, or pick one from `nook get workspaces --json`, which lists
each workspace's `locations` with their node and status.

```
Error: no workspace named 'not-a-repo' — try `nook get workspaces`
```
Names are matched case-insensitively against name, slug and id.

```
Error: no session named 'x' — try `nook get sessions`
```

```
Error: runtime 'claude' is not installed on this node
```
Pick a different `--node`, or a runtime that machine actually reports.

```
Error: session's node is offline
```
The machine dropped after the session started. The tmux session is still there;
it works again when the node reconnects.

**Empty or unchanged `read` output** usually means the runtime is still
thinking. Prefer `exec`, which waits for the screen to settle.
