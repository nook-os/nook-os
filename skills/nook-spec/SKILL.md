---
name: nook-spec
description: "Interview the user about a raw idea until confident, then file a build-ready issue on the NookOS board. Use when asked to run the loop's spec interview, draft a queue-ready issue, or plan a feature. Interactive — requires the user present; never run unattended."
version: 1.0.0
author: NookOS
license: MIT
platforms: [linux, macos]
metadata:
  hermes:
    tags: [NookOS, Board, Kanban, Spec, Planning, Interview, Loop]
    category: autonomous-ai-agents
    related_skills: [nookos, nook-build, nook-review]
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

> **Gap:** the CLI has no create verb yet — there is no `nook create task`.
> Until one exists, POST to the control plane directly, reading the server and
> token the CLI already stores in `~/.config/nook/auth.toml`:

```bash
NOOK_SERVER=$(grep '^server' ~/.config/nook/auth.toml | sed 's/.*"\(.*\)"/\1/')
NOOK_TOKEN=$(grep '^token'  ~/.config/nook/auth.toml | sed 's/.*"\(.*\)"/\1/')

# The board path segment is the board's UUID, NOT its key. Passing the key
# (e.g. "NOOK") does not 404 — it comes back an EMPTY 200, which reads as
# success and files nothing. Resolve the UUID first:
BOARD=$(curl -s "$NOOK_SERVER/api/v1/boards" -H "Authorization: Bearer $NOOK_TOKEN" \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)[0]["id"])')   # first board
# (If there is more than one board, pick the id whose "key" matches the one
#  you want rather than [0].)

curl -s -X POST "$NOOK_SERVER/api/v1/boards/$BOARD/tasks" \
  -H "Authorization: Bearer $NOOK_TOKEN" \
  -H 'Content-Type: application/json' \
  --data-binary @issue.json
```

Report the `key` (e.g. `NOOK-42`) and `url` the API returns. **A response with
no `key` means the POST silently no-op'd — almost always the board key was used
in the path instead of the UUID.** Later skills use that key rather than
guessing it. Confirm with `nook task NOOK-42`.

If the user gave a priority, set it — urgent `1`, high `2`, medium `3`,
low `4`, none `0`. Unset sorts *last*, not first.

If this issue depends on another, record it so the builder skips it until the
blocker is done. **Direction matters and is the opposite of what reads
naturally:** a relation is `from_task blocks to_task`, so `from_task` is the
BLOCKER. Post it on the **blocker**, with `to_task` = the dependent — NOT on
the dependent. `to_task` also takes a UUID, not a key.

```bash
# "MAIN-4 blocks MAIN-5" — post on the blocker (MAIN-4), point at the dependent:
curl -s -X POST "$NOOK_SERVER/api/v1/tasks/<blocker-uuid>/relations" \
  -H "Authorization: Bearer $NOOK_TOKEN" -H 'Content-Type: application/json' \
  -d '{"to_task":"<dependent-uuid>","kind":"blocks"}'
```

Verify the direction landed right: fetch the DEPENDENT and confirm it reports
`is_blocked: true` with the blocker in its `blocked_by` list.

## Hard rule

Never apply the `agent-ready` label. The user applies it on the board after a
final read — that label is the approval gate between "idea" and "an agent
builds it".

> **Currently enforceable only by you.** The MCP door refuses `agent-ready`;
> the REST door behind `nook label` does **not**. You are technically able to
> apply it. Do not: applying it means approving your own work.

