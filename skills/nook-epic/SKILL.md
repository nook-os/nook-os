---
name: nook-epic
description: "Walk opted-in NookOS epics and file the next ready sub-ticket from the epic's own body — unattended, one ticket per pass. Use to run the loop's epic decomposer. Designed for /loop; never interviews the user, escalates to a human when the epic cannot answer."
version: 1.0.0
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

An epic carries a decided architecture and an ordered list of the sub-tickets it
tracks. Normally each follow-up waits for a human to run `/nook-spec` after its
predecessor merges — the loop stalls on ceremony. This skill removes the wait
for epics whose owner has **opted in**: it reads the epic's own body, finds the
next sub-ticket whose dependencies have actually landed, writes that ticket in
the exact `/nook-spec` shape grounded in the epic's architecture plus the
current code, and files it into the backlog — where a human still promotes it,
exactly as with a hand-specced ticket.

It is `/nook-spec` with the interview removed and the source of truth swapped:
the **epic body** answers the product questions a human otherwise would. When
the epic does not answer, this skill does not guess and does not ask inline — it
escalates and moves on.

This is a **decomposer, not a builder and not an interviewer.** It writes
tickets; `/nook-build` builds them and `/nook-review` reviews them, unchanged.
Designed for `/loop`: **one pass files at most one ticket.**

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

## 1. Pick the epic (AC-2)

Scan only epics their owner has opted in, in the order work should be taken:

```bash
nook tasks --type epic --label auto-spec --not-label spec-blocked --backlog --json
```

Each flag is load-bearing:

- `--type epic` — epics are excluded from the default pick server-side; this is
  the only way to list them.
- `--label auto-spec` — the opt-in gate. **An epic without `auto-spec` is never
  touched**, read or written, by this skill.
- `--not-label spec-blocked` — skip an epic already waiting on a human answer,
  exactly as the builder skips `blocked`.
- `--backlog` — an epic is a container and usually lives in the backlog; include
  it.

Results arrive priority-then-oldest (urgent first, no-priority last). Walk the
list in order and take the **first epic that has a ready line** (Section 2). If
an epic has no ready line, move to the next. If none do, say so and end the
pass. Do not invent work.

## 2. Find the next ready line (AC-3)

Read the whole epic and parse its `## Sub-tickets` list (format below):

```bash
nook task NOOK-7 --json     # .description carries the body; .workspace_id, .priority too
```

Each numbered line is a sub-ticket the epic tracks. A line is **filed** when it
carries a `(filed: KEY)` marker. A line's prerequisites are its `(depends: N …)`
markers, naming earlier line numbers.

The **next ready line** is the first line, top to bottom, that is:

1. **not yet filed** (no `(filed:)` marker), AND
2. every `(depends: N)` line is **filed** AND that filed ticket has **landed** —
   it is in a completed or canceled column, not merely filed.

A task's JSON carries only `column_id`, not a column *type*, so don't try to read
the type off the ticket. Ask the server which of the epic's children have landed
— the `--column-type` filter resolves it for you (run it once per terminal type
and union the keys):

```bash
nook tasks --parent NOOK-7 --column-type completed --backlog --json
nook tasks --parent NOOK-7 --column-type canceled  --backlog --json
```

A `(depends: N)` line is landed iff its `(filed: KEY)` ticket appears in that
union.

**Filing early for a blocked line is prohibited.** The whole point is that each
ticket is specced against *merged reality* — the code its predecessor actually
produced — not against a prediction. A line whose dependency is filed but still
in progress is **not** ready; leave it and look no further down this epic (a
later line depending on it cannot be ready either). If the first unfiled line's
dependencies are not all landed, this epic has nothing to do this pass.

Before drafting, run the two guards that force an escalation instead (Section 5):

- **Ambiguous mapping.** List the epic's real children and confirm every one is
  named by some `(filed:)` marker in the body:

  ```bash
  nook tasks --parent NOOK-7 --backlog --json    # every child key must appear as (filed: KEY)
  ```

  A child with no matching marker means the body and the board disagree about
  what has been filed — **never double-file.** Escalate.

- **No workspace.** If the epic's `.workspace_id` is null, a filed child would be
  unbuildable. Escalate.

## 3. Draft the ticket (AC-4)

Draft the ready line as a complete `/nook-spec` issue — same shape, same rules —
using the **epic's architecture section** as the product brain and **reading the
current code** as the codebase brain. Never guess; if the line under-specifies,
escalate (Section 5).

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

- path/to/file.rs — why it matters

## Test expectations

- What should be tested, manually or automatically

## How to verify

1. Numbered manual steps anyone can follow to confirm the work, covering every
   AC.
```

Apply the same rules `/nook-spec` does: every `AC-N`/`NG-N` has a stable id, no
acceptance criterion may require a non-goal, and the ticket is sized to one day
of agent work or less. Before drafting, apply the confidence test:

> Could two different engineers read this spec and ship the same observable
> behavior?

If the epic's architecture plus the current code cannot make that true for this
line, the epic under-specifies it — **escalate (Section 5), do not fill the gap
by guessing.**

## 4. File it (AC-4)

File the drafted ticket **under the epic, into the backlog**, inheriting the
epic's workspace and priority. The default column is the backlog, so do not pass
`--column-type`:

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

It prints the new `key` (e.g. `NOOK-42`). If the drafted line names a dependency
that is a real ticket, record it so the builder waits:

```bash
nook relate MAIN-42 blocks NOOK-42   # <BLOCKER> blocks <DEPENDENT>
```

Then **annotate the epic line** and **comment**, so the next pass sees this line
as filed and a human sees what happened:

```bash
# Append "(filed: NOOK-42)" to that ONE line and write the whole body back.
# set-description reads the current version and guards against concurrent edits;
# pass the full new body as a single argument so its newlines survive.
nook set-description NOOK-7 "<the epic body with (filed: NOOK-42) appended to line N>"

nook comment NOOK-7 "Filed NOOK-42 for sub-ticket N."
```

`nook create task` lands the ticket in the backlog (Triage). **Never apply
`agent-ready`** — a human promotes it from the backlog after a final read,
exactly as with a hand-specced ticket. `task.created` already fans out through
the notification channels, so filing itself notifies; no extra `nook notify` is
needed on the happy path. End the pass.

## 5. Escalate when the epic can't answer (AC-5)

When any of these is true, do **not** file. Comment the exact question on the
epic, block it, and push it through the notification fan-out so a human sees it:

- The next line **under-specifies** — the confidence test (Section 3) fails even
  with the epic's architecture and the current code in hand.
- **Ambiguous mapping** — a child exists on the board that no `(filed:)` marker
  names (Section 2). Filing would risk a duplicate; refuse.
- **No workspace** — the epic's `workspace_id` is null.

```bash
nook comment NOOK-7 "Sub-ticket 2 under-specified: the architecture names a 'catalog gate' but not whether an uncatalogued kind is an error or a silent no-op. Decide, then I can spec it. (Blocking auto-spec.)"
nook label NOOK-7 spec-blocked
nook notify "Epic NOOK-7 needs a decision" \
  --body "Auto-spec is blocked on sub-ticket 2 — see the comment." \
  --level warning --link "<the epic's url>"
```

State the exact decision and the options, never "this is unclear." The human
answers by **editing the epic body** and **removing `spec-blocked`**; the epic
then reappears in Section 1's scan. End the pass.

`nook label` attaches an **existing** label and 404s on an unknown name, so
`spec-blocked` must be one of the board's labels — create it once, the way
`blocked` already exists for the builder. If the add 404s, that is the missing
label, not a missing task.

## 6. Complete the epic (AC-6)

When **every** line carries `(filed:)` **and** every filed ticket is in a
completed or canceled column, the epic's queue is done. Say so, stop scanning it,
and notify:

```bash
nook comment NOOK-7 "Every sub-ticket is filed and landed — the auto-spec queue for this epic is complete."
nook label NOOK-7 --remove auto-spec
nook notify "Epic NOOK-7 complete" --body "All sub-tickets filed and landed." --level success --link "<the epic's url>"
```

Removing `auto-spec` takes the epic out of Section 1's scan for good. End the
pass.

## The `## Sub-tickets` format

The epic body must carry a `## Sub-tickets` section whose lines this skill reads
and, only ever, appends `(filed: KEY)` to. One line per tracked ticket:

```md
## Sub-tickets

1. Events table + record() bridge — (filed: MAIN-84)
2. Notification catalog route — (depends: 1)
3. Settings UI for the catalog — (depends: 2)
```

- The leading integer is the line's number `N` — how `(depends: N)` and the
  `Filed NOOK-42 for sub-ticket N` comment refer to it.
- `(filed: KEY)` — appended by this skill when it files the line. Its presence is
  the sole record that the line is done; nothing else on the line is ever
  edited.
- `(depends: N)` or `(depends: N, M)` — the earlier lines that must be **filed
  and landed** (completed/canceled) before this line is ready. A line with no
  `(depends:)` is ready as soon as it is the first unfiled line.

The architecture the tickets are specced against lives elsewhere in the epic
body (an `## Architecture` section or equivalent); this skill reads it whole.

## Hard rules

- **Never apply `agent-ready`.** Filed tickets land in the backlog; the human
  promotes them, exactly as for hand-specced work. Applying it yourself is
  approving your own work — the load-bearing gate of the whole loop.
- **Never edit a sub-ticket line except to append `(filed: KEY)`.** The epic
  body is the human's document; the only mutation this skill makes to a line is
  that one marker.
- **Never touch an epic lacking `auto-spec`.** No read-and-file, no annotation.
  The label is the opt-in, and it is the human's to give and remove.
- **One ticket per pass.** File one and end; the loop calls this skill again for
  the next. Never file a chain in a single pass — each ticket must be specced
  against the merged reality of the one before it, which does not exist yet.
- **Never guess and never interview.** If the epic cannot answer, escalate
  (Section 5). This skill runs unattended; there is no user in the room.
