---
name: nook-spec
description: "Interview the user about a raw idea until confident, then file a build-ready issue on the NookOS board. Use when asked to run the loop's spec interview, draft a queue-ready issue, or plan a feature. A human answers — live at the terminal, or asynchronously when run as a detached loop job; never fully unattended."
version: 1.3.0
author: NookOS
license: MIT
platforms: [linux, macos]
metadata:
  hermes:
    tags: [NookOS, Board, Kanban, Spec, Planning, Interview, Loop]
    category: autonomous-ai-agents
    related_skills: [nookos, nook-build, nook-review, nook-epic]
---

# Spec interview

Turns a raw idea into a NookOS board issue so complete that a build agent needs
nothing beyond the issue. Works like plan mode: research the codebase,
interview the user in rounds until confident, draft, confirm, file. The user
is the product brain; you are the codebase brain. Never guess product
decisions.

## 0. Preflight

```bash
nook whoami          # must report a user token, not a node token
nook tasks --json    # proves the board is reachable
```

If `whoami` fails or reports a node token, stop and tell the user to mint a
user token in the NookOS UI (Settings → Access tokens) and run
`nook login --token nook_user_…`. Do not continue without it.

## How you ask — terminal, job, or unattended

Every place this skill asks a human — the interview rounds (§2) and the go-ahead
before filing (§4) — routes through one of three channels, chosen by your run
context. The interview logic, gates, and flow are **byte-identical** across all
three; only the ask *primitive* changes:

- **Human at a terminal** (`NOOK_JOB_ID` is NOT in the environment): ask
  interactively, exactly as today — `AskUserQuestion` rounds, and show the full
  draft and wait for a go-ahead in chat.
- **Detached loop job** (`NOOK_JOB_ID` is set — a spec job is running you on an
  executor node): you have no terminal, but a human answers asynchronously.
  Raise each question as a durable interaction and block on it:

  ```bash
  nook interactions ask --wait "Which store backs sessions — Postgres or Redis?" \
    --choice Postgres --choice Redis
  ```

  `--wait` auto-anchors to this job via `NOOK_JOB_ID` — which pauses the job to
  `waiting_on_human` and resumes it when the answer lands — blocks until a human
  answers from any surface, then prints the answer to stdout. Use it for the
  interview rounds **and** the final go-ahead before filing. Never assume an
  answer.
- **Unattended, no job** (no terminal and no `NOOK_JOB_ID`): there is no one to
  ask and nothing to pause on. Do not guess a product decision — file nothing,
  report that the spec needs a human, and end.

### Job mode also has an INPUT channel — the seed and steering messages

Asking is only half of it. In job mode a human can also speak **without being
asked**, and both halves of that channel are yours to read:

- **The seed** is the opening idea — the human's first message, exactly as if
  they had typed it at the terminal. `$NOOK_JOB_SEED` holds it verbatim, line
  breaks intact; your own arguments may carry a flattened copy of the same text
  (the node types `/nook-spec MAIN-42 <the brief>`). Prefer the env var when both
  are present. **Start the interview from the seed**: research against it (§1),
  and let it narrow the first round of questions the way a human's opening
  paragraph does. A job with no seed (`$NOOK_JOB_SEED` unset or empty) starts
  from the ticket alone, exactly as before.
- **Steering messages** arrive as an ordinary turn in your session, unprompted —
  nobody asked a question and none is outstanding. Treat each as new product
  input from the human: fold it into what you have, re-apply the confidence test
  (§2), and continue. It may add scope, cut scope, correct an assumption, or
  answer something you were about to ask; take it as authoritative, the same as
  an interview answer.
- **A steering message is not an answer to an outstanding ask.** If you are
  blocked on `nook interactions ask --wait`, its answer arrives on that command's
  stdout and nowhere else. A message that lands while you wait is extra context,
  not the reply — keep waiting for the real one rather than assuming the question
  was addressed.

Everything else — the research pass, the rounds, the confidence test, the draft
shape, the filing rules — is unchanged by the presence of a seed or a message.

## 1. Research before asking

Read the relevant code first. Find which files are involved, what patterns
already exist, and what constraints apply. Never ask the user something the
codebase can answer.

## 2. Interview in rounds

Ask 1-4 questions per round, each with concrete options and your recommended
option first. Ask only genuine product decisions:

- Behavior forks: who sees it, what exactly happens, where does it live
- Scope boundaries: what is explicitly out of this issue
- Edge cases that change acceptance criteria: empty states, permissions,
  failure handling
- Data implications: existing records, migrations

After each round, fold the answers in and apply the confidence test:

> Could two different engineers read this spec and ship the same observable
> behavior?

If any fork remains, ask another round. There is NO cap on rounds: a small
fix might need two questions; a big feature legitimately needs 10-20+. Never
stop early because it feels like a lot of questions. Once the test passes,
stop — no filler questions.

## 3. Draft the issue

Use exactly this shape:

```md
## Problem

What user or business problem does this solve? One or two sentences.

## Acceptance Criteria

- [ ] AC-1 — Observable, testable outcome one
- [ ] AC-2 — Observable, testable outcome two

## Non-goals

- NG-1 — What must NOT change in this task
- NG-2 — What is explicitly excluded or saved for later

## Relevant files

- path/to/file.ts — why it matters

## Test expectations

- What should be tested, manually or automatically

## How to verify

1. Numbered manual steps anyone can follow to confirm the work: where to
   go, what to do, exactly what should happen. Cover every AC.
```

Rules for the draft:

- Every acceptance criterion is an observable outcome with a stable `AC-N`
  id. Every non-goal has a stable `NG-N` id. These ids are the contract the
  build and review skills enforce.
- No acceptance criterion may require a non-goal. If one does, resolve it
  with the user before filing.
- Size the issue to one day of agent work or less. Bigger work becomes a
  chain of small issues, ordered so each is buildable using only merged
  code from the ones before it.

## 4. Confirm and file

Show the full draft in chat and get the user's go-ahead. Then file it.

**In job mode the gate is the same gate, over the durable channel.** You have no
chat, but you do have a transcript and a message channel, so nothing about
draft-then-file changes:

1. **Print the complete draft first** — every section, verbatim, as it will be
   filed — plus the one-line header (`Workspace:` / `Type:` / priority). Your
   session output is streamed to the job transcript, so printing it is how the
   human reads it. Print it BEFORE you block on anything; a draft stacked behind
   a blocking ask is a draft nobody has seen yet.
2. **Then wait for a go-ahead.** Either channel counts: an answer to
   `nook interactions ask --wait "File this spec as drafted above?" --choice file
   --choice revise` (which pauses the job until it lands), or an unsolicited
   steering message saying to go ahead. A go-ahead is an explicit yes — "file",
   "file it", "yes", "go".
3. **Revise and re-show on anything else.** Changes requested → fold them in,
   print the whole draft again, and ask again. Never file a draft the human has
   only seen an earlier version of, and never file on silence — a job that times
   out or is canceled with no go-ahead files nothing.

**Scope the ticket to a workspace.** A confined `/nook-build` agent only claims
tasks in its own workspace, so a ticket with no workspace is one no loop will
ever build. Assign the workspace you are speccing in:

```bash
WS=$(nook workspace current --json | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d["id"] if d else "")')
```

If `WS` is non-empty, include `"workspace_id": "$WS"` in the issue JSON and show
`Workspace: <name>` in the draft so the user can see and override it. If it is
empty (you are not in a workspace session), say so in the draft and file
unscoped only if the user confirms — an unscoped ticket needs a workspace set on
the board before any loop will pick it up.

Create it with **`nook create task`**. It resolves the board itself (the first
by default; `--board KEY` to pick another) and inherits the session's workspace,
so there are no UUIDs to hand-resolve. The drafted markdown is the description —
feed it on stdin with `--description -`:

```bash
nook create task --title "<the issue title>" --description - <<'EOF'
## Problem
…the whole drafted markdown…
EOF
```

It prints the created `key` (e.g. `NOOK-42`) and `url`; later skills use that key
rather than guessing it. A rejected value (an unknown type, a non-epic parent, a
blank title) exits non-zero with the server's own message. Confirm with
`nook task NOOK-42`.

**A filed ticket lands in the backlog** (Triage — the board's first column), and
the loop cannot pick from the backlog: it stays a human refinement space until
someone sends it to the board (MAIN-80). So `nook tasks` (the default pick) will
NOT show a ticket you just filed — list your backlog with `nook tasks --backlog`.
A ticket is only buildable once a human moves it out of Triage AND applies
`agent-ready`.

If the user gave a priority, pass `--priority` — urgent `1`, high `2`, medium
`3`, low `4`, none `0`. Unset sorts *last*, not first.

**Set the issue type** with `--type` — one of `task`,
`bug`, `epic`, `story`, `chore` (exactly the values the board accepts; anything
else is rejected). Use `epic` for a tracker/roadmap ticket — the kind that
never gets `agent-ready` because it is a parent that decomposes into buildable
children, not a unit of work — and otherwise the best fit: `bug` for a defect,
`story` for user-facing behaviour, `chore` for maintenance/tooling/config, and
`task` as the default when none of those fit. Omitting it defaults to `task`.
Show `Type: <type>` in the draft next to `Workspace:` so the user sees the
classification and can override it before you file.

If this issue depends on another, record it so the builder skips it until the
blocker is done. **Direction matters and is the opposite of what reads
naturally:** in `nook relate <BLOCKER> blocks <DEPENDENT>`, the first argument is
the BLOCKER and the second is what it holds up. Keys or uuids both work, and the
command reports whether the dependent is now blocked so you can confirm the
direction landed:

```bash
nook relate MAIN-4 blocks MAIN-5   # MAIN-4 blocks MAIN-5
```

Kinds `relates` and `duplicates` are also accepted.

## Epics

An **epic** (`--type epic`) is a tracker that other tickets hang off. To file a
ticket under one, pass `--parent <epic key or uuid>` to `nook create task` — the
parent must be a `type='epic'` task **on the same board**, and an epic itself
never has a parent (no nesting):

```bash
nook create task --title "…" --type task --parent NOOK-7 --description - <<'EOF'
…
EOF
```

When you spec a **chain** off an epic — decomposing it into the small buildable
issues the epic tracks — set `--parent` on **every** child so the whole chain is
listable and the epic shows its progress. List an epic's tickets any time with
`nook tasks --parent NOOK-7 --backlog` (a uuid or key), and `nook task NOOK-7`
shows a Children section directly. Detach later with `nook issues set-parent
NOOK-42 none`, or re-file under another epic with `nook issues set-parent
NOOK-42 NOOK-7`.

## Hard rule

Never apply the `agent-ready` label. The user applies it on the board after a
final read — that label is the approval gate between "idea" and "an agent
builds it".

> **Currently enforceable only by you.** The MCP door refuses `agent-ready`;
> the REST door behind `nook label` does **not**. You are technically able to
> apply it. Do not: applying it means approving your own work.

