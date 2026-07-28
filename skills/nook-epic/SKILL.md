---
name: nook-epic
description: "Walk opted-in NookOS epics and draft the next sub-ticket, grounded in the epic's own (free-form) body plus the current code. Attended by default: asks the human when it needs discovery, and shows the finished draft for a go-ahead before filing. Unattended (files without a read, escalates via comments) only when /loop passes the `unattended` flag. One ticket per pass."
version: 2.3.0
author: NookOS
license: MIT
platforms: [linux, macos]
metadata:
  hermes:
    tags: [NookOS, Board, Kanban, Epic, Spec, Planning, Automation, Loop]
    category: autonomous-ai-agents
    related_skills: [nookos, nook-spec, nook-build, nook-review]
---

# Epic decomposer

This skill turns an opted-in epic into its next sub-ticket. It reads the epic's
own body as the product brain and the current code as the codebase brain, drafts
one ticket in the issue shape below (§3), and — attended — shows it for approval
before filing it under the epic in the backlog, where a human still promotes it.

It is the **decompose** stage of the loop: it writes tickets and nothing else.
Building and reviewing are separate stages (`nook-build`, `nook-review`); this
skill never does them. **One pass produces at most one ticket.**

The opt-in is the `auto-spec` label. An epic without it is never touched.

## Mode — read your invocation first

- **Attended** (the DEFAULT — a human just ran `/nook-epic`, so they are in the
  room): when you need discovery — the body is unclear, a line under-specifies,
  or the mapping is ambiguous — **ask them 1–4 questions inline** (concrete
  options, your recommendation first) and unblock in real time. And **always show
  the finished draft and get their go-ahead before you file** (§3a) — never
  auto-file. Keep the human in the loop end to end.
- **Unattended**: your arguments contain `unattended` (the autonomous `/loop`
  passes it), or you were woken by a schedule with no human turn. There is no one
  to ask — when the epic can't answer, **escalate via a comment** (§5b) and end
  the pass. Unattended needs enough structure in the body to be deterministic
  (see §2b); a free-form body it cannot resolve alone is itself an escalation.
- **Detached loop job** (`NOOK_JOB_ID` is set — a decompose job is running you on
  an executor node): there is no terminal, but a human is reachable
  asynchronously. Where attended asks inline (§5a) and unattended escalates by
  comment (§5b), you instead raise a durable interaction and **block** on it —

  ```bash
  nook interactions ask --wait "<the exact decision>" --choice A --choice B
  ```

  — which auto-anchors to this job via `NOOK_JOB_ID`, pauses the job to
  `waiting_on_human`, resumes it when the answer lands, and prints the answer to
  stdout. Everything else — the discovery logic (§2), the confidence test, and the
  file/escalate gates — is **byte-identical**; only the ask primitive changes.

Everything below is shared; the two places the mode matters are §2 (how much you
may infer) and §5 (ask inline · ask durably · comment).

## 0. Preflight

```bash
nook whoami          # must report a USER token, not a node token
nook tasks --json    # proves the board is reachable
```

If `whoami` fails or reports a node token, end the pass and tell the user to
`nook login --token nook_user_…`. A node token cannot drive the board.

You are confined to your workspace: `nook tasks` scopes to the session's
workspace, so you only ever see and touch this repo's epics. If `nook workspace
current` prints nothing you are not in a workspace session — end the pass.

## 1. Pick the epic

Scan only epics whose owner has opted in, in the order work should be taken:

```bash
nook tasks --type epic --label auto-spec --not-label spec-blocked --backlog --json
```

Each flag is load-bearing:

- `--type epic` — epics are excluded from the default pick server-side; this is
  the only way to list them.
- `--label auto-spec` — the opt-in gate. **An epic without `auto-spec` is never
  touched**, read or written, by this skill.
- `--not-label spec-blocked` — skip an epic already waiting on a human answer.
- `--backlog` — an epic is a container and usually lives in the backlog.

Results arrive priority-then-oldest. Walk the list in order and take the **first
epic that has a next ticket to file** (§2). If none do, say so and end the pass.
Do not invent work.

If the human named an epic when they invoked you (`/nook-epic NOOK-7`), take that
one — but still require `auto-spec`; if it is missing, say so and stop (the label
is the opt-in and it is theirs to give).

## 2. Read the epic and find the next ticket

Read the WHOLE epic body — its direction/architecture and whatever it says about
the work still to come. The body is **free-form**: prose, an architecture note, a
loose bullet list, a numbered plan — take it as written.

```bash
nook task NOOK-7 --json     # .description is the body; .workspace_id, .priority too
```

**What is already filed comes from the board, not the body.** The body is the
human's document; you never rely on markers inside it to know state. List the
epic's real children:

```bash
nook tasks --parent NOOK-7 --backlog --json   # every existing sub-ticket of this epic
```

The **next ticket** is the next unit of work the body describes that is **not yet
one of those children** and whose prerequisites have **landed** — a prerequisite
is landed when its ticket is in a completed/canceled column, not merely filed:

```bash
nook tasks --parent NOOK-7 --column-type completed --backlog --json
nook tasks --parent NOOK-7 --column-type canceled  --backlog --json
```

**Never spec against unmerged work.** Each ticket is specced against *merged
reality* — the code its predecessor actually produced. If the next unit depends
on a child that is filed but still in progress, it is not ready: leave it and
look no further down this epic. If nothing is ready, this epic has nothing to do
this pass.

### 2a. Attended — infer, then confirm

Propose the next ticket to the human in one line ("Next up looks like: *the
notification catalog route* — the piece after the events table that just landed.
Spec it?"). If the body clearly implies the next unit, proceeding on a yes is
fine. If it is genuinely ambiguous which unit is next, or two are equally ready,
**ask** (§5a) rather than pick for them.

### 2b. Unattended — require determinism

With no human to confirm, you may only proceed when the body makes the next unit
and its order **unambiguous** — e.g. an ordered list where the next unfiled item
and its dependencies are explicit. If the free-form body leaves the next unit or
its ordering to judgment, that is an escalation (§5b): comment what you need and
stop. Never guess order unattended.

## 3. Draft the ticket

Draft the next unit as a complete issue in exactly this shape, using the **epic
body** as the product brain and the **current code** as the codebase brain:

```md
## Problem

## Acceptance Criteria

- [ ] AC-1 — Observable, testable outcome
- [ ] AC-2 — …

## Non-goals

- NG-1 — What must NOT change
- NG-2 — …

## Relevant files

- path/to/file.rs — why it matters

## Test expectations

## How to verify

1. Numbered manual steps covering every AC.
```

Every `AC-N`/`NG-N` has a stable id, no AC may require a non-goal, and the ticket
is sized to one day of agent work or less. Apply the confidence test:

> Could two different engineers read this spec and ship the same observable
> behavior?

If the epic body plus the current code cannot make that true — **do not fill the
gap by guessing.** Attended: ask (§5a). Unattended: escalate (§5b).

### 3a. Attended — show the draft and get the go-ahead BEFORE filing

**Never auto-file in attended mode.** The human sees the complete drafted issue
and approves it before anything is written to the board — that is the point of
attended mode: you keep them in the loop, you do not file on their behalf.

Show the **full** draft in the chat — every section, verbatim, as it will be
filed — plus a one-line header stating what it is:

> Drafted sub-ticket for **NOOK-7** — *Notification catalog route* (the unit
> after the events table that just landed). Workspace: Nook@OS · priority 3.
> Full draft below; say the word and I'll file it.

Then **end the turn and wait.** Do not call the filing commands in the same turn
as the draft. File only after the human replies with a go-ahead ("file", "file
it", "yes", "go"). If they ask for changes, revise and show it again — still no
filing until they approve. If they decline, don't file; end the pass.

(Unattended has no one to approve, so it skips this gate and files directly —
its safety is the determinism requirement in §2b, not a human read.)

## 4. File it

Once approved (§3a) — or, unattended, once §2b is satisfied — file under the
epic, into the backlog, inheriting the epic's workspace and priority (the backlog
is the default column — do not pass `--column-type`):

```bash
nook create task \
  --title "<the sub-ticket title>" \
  --type task \
  --parent NOOK-7 \
  --workspace "<the epic's workspace id>" \
  --priority <the epic's priority> \
  --description - <<'EOF'
## Problem
…the whole drafted markdown…
EOF
```

It prints the new `key`. If the ticket depends on a real filed ticket, record it
so the builder waits:

```bash
nook relate MAIN-42 blocks NOOK-42   # <BLOCKER> blocks <DEPENDENT>
```

Then leave a trail on the epic — a comment, **not** an edit to the body (the body
is the human's free-form document; do not annotate it):

```bash
nook comment NOOK-7 "Filed NOOK-42 — <the sub-ticket>, the next unit after <what it followed>."
```

**Never apply `agent-ready`** — a human promotes it from the backlog after a
final read. `task.created` already fans out through the notification channels, so
filing itself notifies. End the pass.

> If the epic **already** tracks its sub-tickets as an explicit list in the body
> and the human wants that list kept current, you MAY append a `(filed: KEY)`
> marker to the matching line — but only when such a list already exists. Never
> impose structure on a free-form body.

## 5. When the epic can't answer

### 5a. Attended — ask (the default)

Ask the human via `AskUserQuestion`: 1–4 questions, each stating the exact
decision (never "this is unclear") with concrete options and your recommendation
first. Fold the answers in and continue this pass — draft (§3), show it (§3a),
file on their go-ahead. Unblock now, not next week.

### 5b. Unattended — comment and escalate

No one to ask. Comment the exact question on the epic, block it, and push it
through the notification fan-out:

```bash
nook comment NOOK-7 "Next sub-ticket under-specified: the body describes a 'catalog gate' but not whether an uncatalogued kind is an error or a silent no-op. Decide, then I can spec it. (Blocking auto-spec.)"
nook label NOOK-7 spec-blocked
nook notify "Epic NOOK-7 needs a decision" \
  --body "Auto-spec is blocked — see the comment." \
  --level warning --link "<the epic's url>"
```

State the exact decision and the options. The human answers by **editing the epic
body** and **removing `spec-blocked`**; the epic then reappears in §1's scan. End
the pass.

`nook label` attaches an **existing** label and 404s on an unknown name, so
`spec-blocked` must be one of the board's labels — create it once, like `blocked`
for the builder. A 404 on the add is the missing label, not a missing task.

## 6. Complete the epic

When the body describes no further unfiled work **and** every child is in a
completed/canceled column, the epic's queue is done:

```bash
nook comment NOOK-7 "Every sub-ticket the epic describes is filed and landed — the auto-spec queue for this epic is complete."
nook label NOOK-7 --remove auto-spec
nook notify "Epic NOOK-7 complete" --body "All sub-tickets filed and landed." --level success --link "<the epic's url>"
```

Removing `auto-spec` takes the epic out of §1's scan for good. End the pass.

## Hard rules

- **Never apply `agent-ready`.** Filed tickets land in the backlog; the human
  promotes them. Applying it yourself is approving your own work — the
  load-bearing gate of the whole loop.
- **Never rewrite the epic body.** It is the human's free-form document. The only
  writes you make are new child tickets, `blocks` relations, and comments — plus,
  optionally, a `(filed: KEY)` marker appended to a pre-existing list line (§4),
  never structure you invent.
- **Never touch an epic lacking `auto-spec`.** The label is the opt-in, the
  human's to give and remove.
- **One ticket per pass.** File one and end; the loop calls you again for the
  next. Each ticket must be specced against the merged reality of the one before
  it, which does not exist yet.
- **Attended asks; unattended comments.** Default to attended. Only go silent and
  escalate-by-comment when `/loop` told you `unattended`.
- **Attended never auto-files.** Show the full draft and wait for an explicit
  go-ahead before writing to the board (§3a). Only unattended files without a
  human read, and only when §2b's determinism holds.
