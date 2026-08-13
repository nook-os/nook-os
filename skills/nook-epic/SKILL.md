---
name: nook-epic
description: "Walk opted-in NookOS epics and draft the next sub-ticket, grounded in the epic's own (free-form) body plus the current code. Attended by default: asks the human when it needs discovery, and shows the finished draft for a go-ahead before filing. Unattended (files without a read, escalates via comments) only when /loop passes the `unattended` flag. One ticket per pass."
version: 2.5.0
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

  A detached job is **attended, asynchronously** — not unattended. Take §2a's
  path, not §2b's: confirm and ask rather than requiring determinism, and honour
  the draft-then-file gate (§3a) through the durable channel. Unattended's
  comment-and-escalate route (§5b) is for a `/loop` pass with nobody reachable,
  which is not this.

### Job mode also has an INPUT channel — the seed and steering messages

Asking is only half of it. In a job a human can also speak **without being
asked**, and both halves of that channel are yours to read:

- **The seed** is the human's opening brief for this pass — direction they wanted
  you to have before you started. `$NOOK_JOB_SEED` holds it verbatim, line breaks
  intact; your own arguments may carry a flattened copy (the node types
  `/nook-epic MAIN-7 <the brief>`). Prefer the env var when both are present. It
  is the human speaking, so it ranks with the epic body, not above the board: use
  it to settle which unit is next when §2 leaves that to judgment, to bound scope,
  or to steer the draft. It never overrides what the board says is already filed
  or landed (§2), and it never licenses specing against unmerged work. A job with
  no seed (`$NOOK_JOB_SEED` unset or empty) reads the epic alone, exactly as
  before.
- **Steering messages** arrive as an ordinary turn in your session, unprompted —
  nobody asked a question and none is outstanding. Fold each into the pass as
  authoritative product input, re-apply the confidence test (§3), and continue.
- **A steering message is not an answer to an outstanding ask.** If you are
  blocked on `nook interactions ask --wait`, its answer arrives on that command's
  stdout and nowhere else. A message that lands while you wait is extra context,
  not the reply.

One pass still produces at most one ticket, however much the human says.

Everything below is shared; the two places the mode matters are §2 (how much you
may infer) and §5 (ask inline · ask durably · comment).

## 0. Preflight

```bash
nook whoami          # must show a WORKSPACE — that is the confinement
nook tasks --json    # proves the board is reachable
```

If `whoami` fails or reports **no workspace**, end the pass and tell the user to
`nook login --token nook_user_…`. Either a user token or a node token inside a
managed session satisfies this: the control plane scopes a session's node token
to that session's tenant and workspace, so it reaches ONE repo where a user
token reaches the whole tenant. Unconfined is what disqualifies a run, not the
kind of credential.

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

**Then cross-check every AC against every NG, before the draft is shown.** The
no-AC-may-require-a-non-goal rule above does not enforce itself; this is the
step that carries it out, and it runs on the child ticket you just drafted —
**per child, never once for the epic.** A pass drafts one child, so that is one
grid per pass; if a pass ever produces more than one, each gets its own. An
epic's own scope statement is not a child's NG list, and checking at the epic
level would pass a chain whose individual tickets each contradict themselves.

Take each `AC-N` in turn against **every** `NG-N` — the whole grid, not a skim
of the prose — judging meaning rather than wording: can that AC be satisfied
without violating that NG? Four shapes to look for:

1. an AC requiring a change to a surface an NG freezes;
2. an AC requiring behaviour an NG defers to a later ticket;
3. an AC requiring data or schema an NG excludes;
4. an AC whose only reasonable implementation crosses an NG, even though its
   wording does not name it.

**Report the result even when it is clean**, with the draft, so a child that
skipped the check is distinguishable from one that passed it:

> AC↔NG cross-check (NOOK-7 child 3): no conflicts (AC-1…AC-4 × NG-1…NG-2).

**On a conflict, hold that child and get a human to rule.** Do not reword either
side, drop one, or pick a winner — the decomposer is not the product brain, and
a contradiction filed here becomes a build that ships confidently wrong.
Name the pair and state the contradiction, then route it the way this run's
context routes any question it cannot answer: attended, ask inline (§5a); in a
job, the durable channel (§3b's `nook interactions ask --wait`); unattended,
escalate by comment (§5b) and file nothing. A canceled or timed-out ask files
nothing either. Fold the ruling in, re-run the grid over the amended child, and
continue.

The check and its result belong to the conversation, the escalation comment or
the job transcript — **never to the filed ticket body.** §4 files exactly the
template above.

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

### 3b. Job mode — the same gate, over the durable channel

A detached job has no chat, but it has a transcript and a message channel, so the
gate holds rather than degrading to unattended's auto-file:

1. **Print the complete draft first** — every section, verbatim, as it will be
   filed, plus the one-line header naming the epic, the unit, workspace and
   priority. Your session output is streamed to the job transcript, so printing
   it is how the human reads it. Print it BEFORE you block on anything; a draft
   stacked behind a blocking ask is a draft nobody has seen yet.
2. **Then wait for a go-ahead** on either channel: an answer to
   `nook interactions ask --wait "File this sub-ticket as drafted above?" --choice
   file --choice revise`, or an unsolicited steering message saying to go ahead
   ("file", "file it", "yes", "go").
3. **Revise and re-show on anything else** — fold the changes in, print the whole
   draft again, ask again. Never file a version the human has not seen, and never
   file on silence: a job that times out or is canceled without a go-ahead files
   nothing.

## 4. File it

Once approved (§3a in a terminal, §3b in a job) — or, unattended, once §2b is
satisfied — file under the
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
  go-ahead before writing to the board (§3a in a terminal, §3b in a detached job).
  Only unattended files without a human read, and only when §2b's determinism
  holds.
