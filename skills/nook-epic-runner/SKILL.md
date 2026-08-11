---
name: nook-epic-runner
description: "Merge-manage one named epic: consume the PRs and verdicts the build/review loops produce, merge what their evidence clears, and close the epic with a follow-up issue when everything is in. Runs NO builds and NO reviews itself — it fully depends on the loops running outside it. The loop's only merge authority; stops for humans on anything meaningful."
version: 1.3.0
author: NookOS
license: MIT
platforms: [linux, macos]
metadata:
  hermes:
    tags: [NookOS, Board, Kanban, Merge, Orchestration, Pull-Request, CI, Loop]
    category: autonomous-ai-agents
    related_skills: [nookos, nook-build, nook-review]
---

# Epic runner

Take **one named epic** and manage its issues to **merged**: as the build and
review loops — running on their own, outside this skill — open PRs and post
verdicts for the epic's `agent-ready` issues, the runner merges what their
evidence clears, keeps the board honest, and closes the epic with a follow-up
issue when everything is in. That is the whole job: **managing the merge of
existing, human-approved, loop-produced work.**

**This skill runs no loop stages itself.** It never claims a ticket, never
writes code, never repairs a PR, and never posts a review verdict — those are
`nook-build`'s and `nook-review`'s jobs, running as their own `/loop` passes
elsewhere. The runner only consumes what they produce. If their output isn't
there yet, the runner reports and waits; it does not do their work for them.

It is, deliberately, the loop's **merge authority** — the one exception to the
rule the sibling skills enforce. `nook-build` never merges; `nook-review` calls
`loop-approved` "evidence for a human, not merge authorization." The runner is
that human's delegate for one epic at a time: the human approved every unit of
work by tagging it `agent-ready`, and authorizes the run by invoking this skill
with the epic key. The authority is bounded — only PRs closing this epic's
children, only on the loops' clean evidence and green required checks — and it
**stops for a human on anything meaningful** rather than guessing.

## Invocation

```
/nook-epic-runner NOOK-7
```

The epic key is **required**. Never scan for epics; naming the epic is the
authorization for the run. If invoked without a key, ask for one and stop.

Designed for `/loop`: one pass merges everything currently eligible, reports
the rest, and ends. Re-invocation resumes from live state.

## 0. Preflight

- `nook whoami` must show a **workspace**; `gh auth status` must pass. A user
  token or a node token inside a managed session both satisfy it — the latter is
  scoped by the control plane to that session's tenant and workspace. No
  workspace means unconfined, which disqualifies the pass.
- You must be in a workspace session (`nook workspace current` prints one) and
  the epic's `workspace_id` must be this workspace — the runner never merges
  another repo's work.
- The named task must exist and be `type: epic`. Anything else: say so, stop.
- The repository must have **required checks configured**. No required CI means
  no merge evidence; stop and require a human before the first merge.

The runner needs **no working tree and no branch** — it changes nothing in the
repository except by `gh pr merge`. Any preflight failure ends the pass with
the reason.

## 1. Survey the epic and announce the run

Read the whole epic (`nook task NOOK-7 --json`) and list its children:

```bash
nook tasks --parent NOOK-7 --backlog --json
```

Classify every child:

- **done** — completed/canceled column (or archived)
- **queued for the loops** — `agent-ready`, not `blocked`, unassigned,
  unblocked, in a board (non-backlog) column: the build loop will take it;
  nothing for the runner to do but wait
- **in flight** — claimed by a builder, or has an open PR referencing it
- **waiting on a human** — `blocked`, `needs-human-review`, still in the
  backlog, or blocked by an unfinished ticket
- **out of reach** — not `agent-ready`. The runner NEVER touches these and
  never applies `agent-ready` itself. If the epic can't complete without them,
  that surfaces as the end-state stop condition — never as a wider queue.

On the first pass of a run, comment the plan on the epic:

```bash
nook comment NOOK-7 "Epic run started: N queued for the loops (KEY…), M in flight, K waiting on a human, J done. The build/review loops produce the work; this run merges what their evidence clears. Stop conditions armed: security, scope conflict, missing CI, human-review flags."
```

## 2. The pass — consume loop output, merge what's cleared

Re-derive everything from the board and GitHub; never from memory.

**Board flow, unattended.** Each stage's owner moves the card: the build loop
moves it to In Progress on claim and In Review on submit; the runner hands the
PR to `main`'s merge queue, and `merge_reconcile` moves the card to **Done when
the queue actually merges it** (MAIN-541 — the runner cannot see that far).
Between the loops, the runner and the control plane, a ticket flows
Todo → In Progress → In Review → Done with no human touching the board.

**Reconcile stragglers first.** Before looking at open PRs, find any child
whose PR is already **merged** (whoever merged it — this run, a prior run, or
a human by hand) but whose card is not in a completed column: move it to Done,
prune its worktree, and comment the reconciliation on the ticket. The board
must keep flowing even when merges happen outside the runner.

Then, for each open PR closing one of this epic's children (oldest first),
read its labels and its latest `Loop review of COMMIT_SHA` verdict, and act
by state:

- **`loop-approved`, verdict clean at the current head** → merge it (§3),
  which since MAIN-541 means hand it to `main`'s merge queue.
- **Already in the merge queue** → in flight, and enqueueing it twice is the
  only way to make that worse. Leave it; report "queued, position N."
- **Ejected by the merge queue** → report it by number with the forge's own
  reason, verbatim. Leave it: repairing an ejected PR belongs to the loops on a
  later card, and the runner never re-enqueues at the head that was ejected.
  Not a stop condition — but never a silent drop either.
- **No verdict yet, or verdict SHA ≠ current head** → the review loop hasn't
  caught up. Leave it; report "awaiting review."
- **`loop-changes-requested`** → the build loop repairs it outside this run.
  Leave it; report "awaiting repair."
- **`needs-human-review`** → stop condition (§4).
- **Required checks pending** → leave it; report "awaiting CI."
- **Merge conflict / not mergeable** → leave it for the loops (the reviewer
  records conflicts as must-fix `[DEFECT]`s and the builder resolves them).
  The runner never pushes to a PR branch — not even a rebase.

The runner's own judgment is confined to **coordination**: merge order among
simultaneously-eligible PRs (dependency edges first, then oldest), and whether
a situation matches a stop condition. Order choices that aren't obvious get
recorded:

```bash
nook comment NOOK-7 "Decision (low-risk): merging KEY-A before KEY-B — A is B's blocker on the board."
```

**Stall watch.** If the same PR has accumulated must-fix verdicts at three or
more distinct head SHAs, the build/review loops are churning without
converging — that is a stop condition, not something to wait out silently.

## 3. The merge — the only place it happens

Merge only when ALL of these hold, re-verified in one final pass immediately
before merging:

- The PR's `Closes KEY` resolves to a child of **this** epic.
- The PR carries `loop-approved` and its latest `Loop review of COMMIT_SHA`
  verdict has zero must-fix findings and no escalation — and that SHA
  **equals the current `headRefOid`** (re-fetch; any drift = wait for
  re-review, no merge).
- `gh pr checks --required` all green; `mergeable` is clean.
- No `needs-human-review` label on the PR.

First ask the forge where the PR stands with the queue — the same read
`nook-yolo` uses, and for the same reason: a PR already queued must not be
queued again, and one the queue ejected must be reported rather than re-fed to
it.

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

`mergeQueueEntry` non-null is "still queued". "Ejected" is the newest
`RemovedFromMergeQueueEvent` whose `reason` is **anything but `merged`** —
`failed_checks`, `manual`, whatever GitHub adds — with the head commit's
`committedDate` **not** later than that event's `createdAt`, which is what says
the branch has not been pushed since. Both are reports, not merges (§2).

**Do not compare `beforeCommit` to `headRefOid`.** It is the base commit the
merge group was built on, usually another pull request's, so that equality never
holds; a rule built on it reads every ejection as "the head moved" and re-feeds
the PR to the queue at the head it was just rejected at. And a removal event
alone means nothing — a *successful* queue merge emits one too, with
`reason: "merged"`. Where the timestamps cannot prove the head moved, treat it
as ejected: a skipped fresh head costs one pass, a re-enqueued dead one repeats
forever.

Otherwise:

```bash
gh pr merge NUMBER --squash --delete-branch
```

Squash, matching the repository's one-commit-per-PR history. Never `--admin`,
never force, never around branch protection — and `--admin` is now also the
flag that skips the merge queue, so it is the one way the runner could land
un-queue-built code on `main`. A refused merge is a stop condition, not an
obstacle to route around.

**That command ENQUEUES; it does not merge (MAIN-541).** `main` requires a
merge queue, so a zero exit says GitHub accepted the PR into the queue. The
queue builds it against the entries ahead of it and merges it minutes later —
or ejects it — long after this pass has ended. The runner never sees which.

So, immediately after, same pass:

- `nook comment KEY "Queued: <pr url> (epic run NOOK-7)."` — **queued**, never
  "merged".
- **Do not move the card, do not prune the worktree, do not claim a merge.**
  The card moves when the PR really merges, and `merge_reconcile` in the
  control plane does it: always running, it asks the forge how each PR actually
  ended and moves the card exactly once. This step used to be the runner's one
  licence to move a card to Done; it now belongs to the component that can
  actually observe the merge. §2's straggler reconciliation is unchanged and
  still moves cards whose PR is **observed merged**, which is where an epic run
  catches up.
- **Do not wait for the queue** — no poll, no re-check, no sleep. The pass ends
  here and the next one re-derives everything, the queue's verdict included.

## 4. Stop conditions — human required

Stop the **entire run** — not just the one PR — when any of these appears in
the loops' output or the board state:

- Any `[SECURITY]` finding in a verdict, at any severity, regardless of which
  group the reviewer put it in.
- Any `[SCOPE-CONFLICT AC-N ↔ NG-N]` verdict, or `needs-human-review` on any
  of the epic's PRs.
- No required checks configured, CI infrastructure itself broken, or a merge
  refused by branch protection.
- Review/repair churn: must-fix verdicts at three or more distinct SHAs on one
  PR (§2's stall watch).
- Every remaining child is **out of reach** (not `agent-ready`) or **waiting
  on a human** — the loops have nothing to work with and the runner cannot
  finish the epic alone.

On stop: comment the exact situation and the decision needed on the epic (and
the PR, when one is involved), then notify and end:

```bash
nook comment NOOK-7 "Epic run STOPPED on KEY: [SECURITY] <exact finding from the verdict>. Decision needed: <the question, with options>. Nothing further was merged."
nook notify "Epic run NOOK-7 stopped — human needed" \
  --body "<one-line reason>. See the epic comment." \
  --level warning --link "<epic url>"
```

State the exact decision and the options, never "this is unclear." The human
resolves it and re-invokes the runner; §1's survey picks up from live state.

If nothing is eligible and nothing is stopped — the loops simply haven't
produced output yet — that is **not** a stop: report the per-PR/per-ticket
state plainly and end the pass. The next pass re-checks.

## 5. Close the epic

When every child is in a completed/canceled column, no epic PR remains open,
and the default branch holds every merge:

1. **Verify, don't assume**: re-list the children and confirm each merged PR
   is in the default branch's history
   (`gh pr view NUMBER --json state,mergeCommit`); confirm required checks on
   the default branch tip are green. Any discrepancy is a stop condition. A PR
   this run *queued* is not a PR that merged — it is still open, so the epic is
   simply not closeable yet, and that is a plain report rather than a stop.
2. Close the epic: move it to the completed column and comment the closing
   summary — issues merged (key → PR), decisions recorded, anything canceled.
3. **File the follow-up issue** (the run's only write that isn't a merge or a
   board move): collect every "Should fix soon" group from the run's review
   verdicts, deferred items and non-goals worth revisiting, and recommended
   improvements observed across the epic's PRs. File ONE ticket:

   ```bash
   nook create task \
     --title "Follow-ups from NOOK-7: deferred work, tech debt, improvements" \
     --type chore \
     --workspace "<the epic's workspace id>" \
     --description - <<'EOF'
   ## Problem
   NOOK-7 merged; these are the items deliberately not blocking it.
   ## Items
   - [ ] <source: KEY/PR — the item, why deferred>
   …
   EOF
   nook relate NOOK-7 relates <NEW-KEY>
   ```

   It lands in the backlog, is **never** labeled `agent-ready`, and names its
   sources so a human can promote line-items into real specs.
4. Notify:

   ```bash
   nook notify "Epic NOOK-7 complete" \
     --body "All issues merged and verified. Follow-ups filed as <NEW-KEY>." \
     --level success --link "<epic url>"
   ```

## Hard rules

- **Never run a loop stage.** No claiming, no building, no repairing, no
  review verdicts, no pushes to any branch — the loops do that outside this
  skill, and the runner consumes their output. If the output is missing, wait
  and report; never substitute.
- **One epic per run, named by the human.** The key in the invocation is the
  entire authorization. Never touch another epic's tickets or PRs.
- **Merge only on the loops' evidence** (§3's full checklist), only via
  squash, never with `--admin` or around branch protection.
- **Never claim a merge the runner did not observe.** `gh pr merge` returning
  zero means the PR entered `main`'s merge queue and nothing more. The word
  "merged" belongs to a PR the forge reports as merged, and moving its card
  belongs to `merge_reconcile`.
- **Never apply `agent-ready`.** The runner merges the queue the human
  approved; it never grows that queue. The follow-up issue ships without it.
- **Stop, don't guess** (§4). Security findings stop the run unconditionally.
  A stopped run is a successful run that found its limit.
- **Record every decision** the run makes on the epic — a run whose choices
  can't be audited from the epic's comments did it wrong.
- **Re-derive state every pass** from the board and GitHub. The run must be
  resumable after any interruption with no memory of the previous process.
