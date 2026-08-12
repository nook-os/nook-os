---
name: nook-yolo
description: "Merge the workspace's loop-approved PRs, unattended, all night. Consumes what the build and review loops produce and lands what their evidence clears — board-wide, not one epic. Runs NO builds and NO reviews itself, writes no code, and never pushes. Skips and records anything it cannot land; never halts the night."
version: 1.2.0
author: NookOS
license: MIT
platforms: [linux, macos]
metadata:
  hermes:
    tags: [NookOS, Board, Kanban, Merge, Pull-Request, CI, Loop, Unattended]
    category: autonomous-ai-agents
    related_skills: [nookos, nook-build, nook-review, nook-epic-runner]
---

# Yolo — the overnight merge authority

One job: **land the PRs the loops have already produced and approved**, across
the whole workspace, unattended, for hours. `nook-epic-runner` does this for one
human-named epic; this does it board-wide, and where the runner **halts the run**
on trouble, yolo **skips the PR, records why, and keeps going** — a night that
stops at 1am on one bad PR is a night wasted.

**It does not code. It does not review.** It never claims a ticket, never writes
a line, never repairs a PR, never posts a verdict, never pushes to any branch —
not even a rebase. Those are `nook-build`'s and `nook-review`'s jobs, running as
their own `/loop` passes alongside this one. Yolo consumes their output and
nothing else. If that output isn't there, yolo waits and reports; it never
substitutes.

Its authority is entirely **derived from gates a human controls**: a ticket only
becomes eligible because a human applied `agent-ready`, and a PR only becomes
eligible because the review loop found it clean. Yolo adds no judgment about
whether work is good — only about whether the evidence is complete.

## Invocation

```
/loop 20m /nook-yolo
```

No arguments. One pass merges everything currently eligible, records the pass on
the night's ledger ticket, and ends. Every pass **re-derives all state** from the
board and GitHub — nothing carries between passes, so the run is resumable after
any interruption, crash, or restart.

## 0. Preflight

- `nook whoami` must report a **workspace**. That line is the confinement: it is
  what stops a pass merging another repo's work. Either a user token or a node
  token inside a managed session satisfies it. No workspace → end the pass.
- `gh auth status` must pass, and the repo must be the workspace's repo.
- The repository must have **required checks configured**. No required CI means
  no merge evidence at all: notify once and end the pass without merging
  anything. This is the one preflight failure worth waking someone for.

Yolo needs **no working tree and no branch**. It changes nothing in the
repository except by `gh pr merge`, and nothing on the board except card moves,
comments, and the two labels named in §3.

## 1. Open the night's ledger

The whole night is one readable thread on the board. Find or create it — by
title, because a pass keeps no memory of the last one:

```bash
DAY=$(date -I)
nook tasks --backlog --json | \
  python3 -c "import sys,json;print(next((t['key'] for t in json.load(sys.stdin) if t['title']==f'Yolo run $DAY'),''))"
```

If empty, create it:

```bash
nook create task --title "Yolo run $DAY" --type chore --description - <<'EOF'
## What this is

The overnight merge ledger. `/nook-yolo` appends one comment per pass: what it
merged, what it skipped and why. Read the thread top to bottom for the night.

Nothing here is a unit of work. Never label it `agent-ready`.
EOF
```

It lands in the backlog (Triage) and stays there. **Never label it
`agent-ready`** — it is a log, not work.

## 2. Reconcile the board first

Before looking at anything mergeable, make the board match reality. A merge that
happened outside this run — a prior pass, a human by hand, an epic-runner — must
still reach Done, or the board lies all night.

For every ticket in this workspace sitting in **In Review** (or any non-completed
column) whose PR is **merged**:

- `nook issues move KEY completed` — the card lands in Done.
- `nook issues prune-worktree KEY` if one is recorded.
- `nook comment KEY "Reconciled: PR <url> is merged; card moved to Done."`

This is granted autonomy, not a judgment call: the merge is already real, so the
board is what catches up.

## 3. The sweep — merge what the evidence clears

List open PRs and work them **oldest first**, except that a PR whose ticket
**blocks** another eligible PR's ticket on the board goes first. When the order
is not obvious, record it:

```bash
nook comment <LEDGER> "Decision: queued MAIN-41 before MAIN-43 — 41 blocks 43 on the board."
```

### Eligibility — all eleven, re-verified immediately before the merge

A PR is eligible only when **every one** of these holds:

1. Open and **not a draft**.
2. Its body carries `Closes <KEY>`, and that key resolves to a task in **this**
   workspace.
3. The ticket carries **`agent-ready`**. This is the human approval gate, and it
   is where yolo's authority comes from. A PR a human wrote by hand, against a
   ticket that never passed the gate, is that human's to merge — skip it and
   report "not loop work."
4. The ticket does **not** carry `blocked`.
5. Neither the PR nor the ticket carries **`needs-human-review`**.
6. The PR carries **`loop-approved`**.
7. Its latest `Loop review of COMMIT_SHA` verdict has **zero must-fix findings**
   and no escalation.
8. That verdict SHA **equals the current `headRefOid`** — or the drift is
   docs-only by the rule below.
9. `gh pr checks --required` — all green. Pending is not green; missing is not
   green.
10. `mergeable` is clean.
11. Nothing in §4's ladder applies.

Re-fetch `headRefOid` and the checks **in one final pass immediately before**
`gh pr merge`. Any drift discovered there is a skip, not a merge.

### The docs-only drift exception

This is the **only** place yolo lands bytes a reviewer never saw, so the test is
mechanical and path-based — never a judgment about whether a change "looks
harmless":

```bash
git diff --name-only <verdictSHA>..<headRefOid>
```

Merge only if **every** path matches `*.md`, `docs/**`, `LICENSE*`, or `*.txt`.
One path outside that list — including a comment-only or whitespace-only change
to a source file — is a skip. Record the exception when taken:

```bash
nook comment KEY "Queued past verdict <verdictSHA> → <headRefOid>: drift is docs-only (README.md, docs/foo.md)."
```

### What the queue did with a PR it was given

A queued PR leaves the queue one of two ways, and yolo learns which on a *later*
pass by asking the forge — never by remembering. One read answers it:

```bash
gh api graphql -f query='
query($o:String!,$r:String!,$n:Int!){
  repository(owner:$o,name:$r){ pullRequest(number:$n){
    merged headRefOid
    mergeQueueEntry{ state position }
    commits(last:1){ nodes{ commit{ oid committedDate } } }
    timelineItems(last:20, itemTypes:[REMOVED_FROM_MERGE_QUEUE_EVENT]){
      nodes{ ... on RemovedFromMergeQueueEvent { reason createdAt } } } } } }' \
  -F o=OWNER -F r=REPO -F n=NUMBER
```

Read it in this order:

1. `merged: true` — it landed. Nothing owed here; the card is `merge_reconcile`'s
   to move, or §2's.
2. `mergeQueueEntry` non-null — **still queued.** Skip; report "queued,
   position N." Enqueueing a PR that is already in the queue is the one action
   that makes this worse.
3. Otherwise take the **newest** `RemovedFromMergeQueueEvent` and read its
   `reason`:
   - **No removal event at all** — this PR has never been through the queue.
     An ordinary candidate; work it through the eligibility list above.
   - **`reason: "merged"`** — the queue merged it, and a removal event is
     exactly what a *successful* queue merge emits. Step 1 already answered
     this; it is listed here because treating any removal event as an ejection
     is the mistake that turns every merge into a false alarm.
   - **Any other reason** — `failed_checks`, `manual` (a human pulled it out),
     or whatever else GitHub adds — the PR **left the queue without merging.**
     Then one comparison decides whether that verdict is about the head in
     front of you: if the head commit's `committedDate` is **after** the
     removal's `createdAt`, the branch was pushed since and this is an ordinary
     candidate again. Otherwise it is **ejected at this head** — ladder row.

**`beforeCommit` is not this PR's head, and comparing it to `headRefOid` is the
trap.** It is the base commit the merge group was built on — usually a *different*
pull request's commit, different again for each ejection of the same PR — so the
equality never holds and every ejection would read as "the head moved," handing
the PR straight back to `gh pr merge` at the head the queue just rejected, every
pass, forever. The timestamps are the honest comparison; there is no field on a
removal event carrying the head it was queued at.

Where the timestamps cannot prove the head moved, **treat it as ejected.**
Skipping a genuinely fresh head costs one pass and the next one picks it up;
re-enqueueing an ejected head costs a queue build and repeats forever.

The reason is GitHub's own `reason` string, quoted verbatim; it is nullable, and
yolo never fills the gap with a guess. "Ejected from the merge queue; the forge
stated no reason" is a true report.

### The merge — which ENQUEUES the PR, and does not merge it

```bash
gh pr merge NUMBER --squash --delete-branch
```

Squash, matching this repository's one-commit-per-PR history. **Never `--admin`,
never force, never around branch protection.** `--admin` is now also the flag
that *skips the merge queue*, which makes it the one way a pass could put
un-queue-built code on `main`; the rule is unchanged and it matters more. A
refused merge is a ladder row (§4), not an obstacle to route around.

**A zero exit means QUEUED, not merged (MAIN-541).** `main` requires a merge
queue, so that command hands the PR to GitHub, which builds it against the
entries ahead of it and merges it minutes later — or ejects it. Both outcomes
land long after this pass has ended, so the exit code is evidence of an enqueue
and of nothing else.

Immediately after, same pass:

- `nook comment KEY "Queued: <pr url> (yolo run $DAY)."` — **queued**, never
  "merged".
- **Do not move the card, do not prune the worktree, do not claim a merge.** The
  card moves when the PR really merges, and `merge_reconcile` in the control
  plane does that: always running, it asks the forge how each PR actually ended
  and moves the card exactly once. Moving it here would assert a merge nobody
  has observed — and when the queue ejects a PR, that board would show shipped
  work that is still open.
- **Do not wait for the queue.** No poll, no re-check, no sleep, however short.
  A night that blocks on queue builds merges one PR an hour. The pass ends here;
  the next pass re-derives what the queue did, like it re-derives everything.

§2's reconciliation is unchanged and still right: it moves cards whose PR is
**observed merged**, which is the honest version of the move this step used to
make on faith. `merge_reconcile` usually gets there first; both are idempotent,
so whichever arrives second finds nothing to do.

## 4. The ladder — every blocked cause has exactly one action

This table **is** the decision. There is no free judgment here: if a situation
matches a row, take that row's action, record it, and move to the next PR.
Nothing in this section ever halts the night.

| Cause | Action |
|---|---|
| Merge conflict / not mergeable | Skip. Comment "conflicting — awaiting repair" on the PR. Add `loop-changes-requested` so the build loop picks up the repair. |
| Required check **failed** | Skip. Comment the **exact failing check name** on the PR. Add `loop-changes-requested`. |
| Required checks **pending** | Skip. Report "awaiting CI." No comment, no label — it is simply not done yet. |
| PR **still in the merge queue** | Skip. Report "queued, position N." No comment, no label — it is in flight. Never enqueue a PR twice. |
| PR **ejected by the merge queue**, head unchanged since | Skip. Name it on the ledger with the forge's `reason`, verbatim (§5). No label, no comment, no re-enqueue: repairing an ejected PR is a later card's job, and yolo's duty is only that it is never silently dropped. |
| No `Loop review of …` verdict | Skip. Report "awaiting review." No comment, no label. |
| Verdict SHA ≠ head, drift **not** docs-only | Skip. Report "awaiting re-review." |
| Verdict has must-fix findings | Skip. Report "awaiting repair" — the build loop owns it. |
| Ticket labeled `blocked` | Skip. Report "ticket blocked — a human owes it an answer." |
| Ticket not `agent-ready` | Skip. Report "not loop work." |
| `needs-human-review` on PR or ticket | Skip. Report it. Never remove the label. |
| **No required checks configured** | Skip. Add `needs-human-review` to the PR. `nook notify --level warning`. |
| Merge **refused by branch protection** | Skip. `nook notify --level warning` with the exact refusal. Never retry, never `--admin`. |
| `[SECURITY]` finding in the verdict, **any severity, any group** | Skip. Add `needs-human-review` to the PR. Comment the **exact finding, verbatim** on both the PR and the ticket. `nook notify --level warning`. **Then continue with the next PR.** |

**Security does not halt the night, and it does not soften either.** One flagged
PR is parked for a human and the other nine still land. But a security finding is
*never* something yolo weighs, discounts, or decides is fine — it is tagged for a
human and left alone, full stop. That is the whole rule: **if it's a security
concern, a human rules on it; yolo does not merge it.**

## 5. Record the pass

Every pass appends one comment to the ledger, whether or not it merged anything.
The ledger **is** the morning report — there is no separate summary:

```bash
nook comment <LEDGER> "Pass <HH:MM> — queued: MAIN-41 (#41), MAIN-42 (#42). Skipped: MAIN-43 (#43, required check \`rust\` failed), MAIN-44 (#44, awaiting review), MAIN-45 (#45, ejected from the merge queue: <the forge's reason>). Nothing else eligible."
```

Name every skip **with its cause**. A ledger line saying "skipped 3" is a line
that has to be re-investigated in the morning, which defeats the point.

**"Queued" is the strongest word a pass may use about work it handed to the
queue.** The ledger is read in the morning as a record of what shipped; a line
saying "merged" about a PR the queue later ejected turns that record into a
lie. What actually merged is on the board, put there by `merge_reconcile`.

### Notifications are rare on purpose

Push only for the things a human must rule on — the last three rows of §4's
ladder (no required CI, protection refusal, `[SECURITY]`), and nothing else.
**No push per merge.** A phone that buzzes nine times a night gets silenced, and
then the one buzz that mattered is missed too.

One exception: the **first fully dry pass** — nothing eligible and no open loop
PRs at all — notifies once, pointing at the ledger, so you know the night is
done rather than stalled:

```bash
nook notify "Yolo run $DAY — queue is dry" \
  --body "Nothing left to merge. The night's ledger is <LEDGER>." \
  --level success --link "<ledger url>"
```

Later dry passes stay silent. Re-derive "already notified" by reading the
ledger's own comments — no memory between passes.

## Hard rules

- **Never write code, never push, never rebase, never repair.** Not one commit,
  not to any branch. If a PR needs work, the ladder hands it to the build loop.
- **Never post a review verdict** or run a review pass. `loop-approved` is
  produced by `nook-review`, elsewhere. Yolo reads it; it never writes it.
- **Never merge without `loop-approved`.** No amount of green CI, clean
  mergeability, or obvious-looking diff substitutes for the verdict.
- **Never apply `agent-ready`** — not to a ticket, not to the ledger. It is the
  human gate yolo's entire authority rests on; applying it would be approving its
  own queue.
- **Never remove `blocked` or `needs-human-review`.** A human put it there; a
  human takes it off.
- **Never re-run CI, never close a PR, never edit a ticket's scope.** A red check
  is a ladder row, not something to retry into greenness.
- **Never claim a merge yolo did not observe.** `gh pr merge` returning zero
  means the PR is in the queue, nothing more. The word "merged" belongs to a PR
  the forge reports as merged, and moving its card belongs to `merge_reconcile`.
- **Never halt the night.** Every trouble is a skip plus a record. The run ends
  when `/loop` stops firing, not because yolo gave up.
- **Re-derive every pass** from the board and GitHub. A pass that trusts a
  previous pass's belief about a SHA, a label, or a check will eventually merge
  something stale.
