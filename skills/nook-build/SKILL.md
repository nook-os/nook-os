---
name: nook-build
description: "Claim the next safe agent-ready issue from the NookOS board, implement it, and open a PR. Use when asked to run the loop's builder, work the approved queue, or fix loop review feedback. Designed for /loop; one pass does one unit of work."
version: 1.0.0
author: NookOS
license: MIT
platforms: [linux, macos]
metadata:
  hermes:
    tags: [NookOS, Board, Kanban, Build, Automation, Pull-Request, Loop]
    category: autonomous-ai-agents
    related_skills: [nookos, nook-spec, nook-review, nook-epic]
---

# Loop builder

One pass = one unit of work: fix review feedback on one existing PR, or build
one issue end to end. Under `/loop`, each iteration runs this skill once.

The board is NookOS. PRs and CI stay on GitHub.

## 0. Preflight

Before changing the board, GitHub, branches, or files:

- `nook whoami` must show a **workspace** — that line is what confines this
  pass to one repo. Either a **user token** (`nook login --token nook_user_…`)
  or a **node token inside a managed session** satisfies it: the control plane
  scopes a session's node token to that session's tenant and workspace, which
  reaches ONE repo where a user token reaches the whole tenant. If `whoami`
  reports no workspace, end the pass — unconfined means the pick can return
  another repo's cards.
- Confirm this is the intended GitHub repository and `origin` is reachable.
- Detect the repository's default branch with
  `gh repo view --json defaultBranchRef --jq .defaultBranchRef.name`; never
  assume it is `main`.
- Require a clean working tree (`git status --porcelain` must be empty). If it
  is dirty, report the paths and end the pass. Never stash, reset, overwrite,
  or commit unrelated work.

## 1. Review feedback first

List open PRs labeled `loop-changes-requested`, including their labels:

```bash
gh pr list --state open --label loop-changes-requested --json number,title,headRefName,headRefOid,labels,updatedAt,url
```

Skip every PR carrying `needs-human-review`; it has left the automated repair
queue until a human resolves the escalation.

If any PR remains, choose the least recently updated one. Read its linked
board issue (`nook task KEY`) and the latest `Loop review of COMMIT_SHA`
verdict in that issue's comments. Its card is sitting in **In Review**; move it
back to In Progress so the board shows it is being worked again (the endpoint
resolves the name to the `started`-type column):

```bash
NOOK_SERVER=$(grep '^server' ~/.config/nook/auth.toml | sed 's/.*"\(.*\)"/\1/')
NOOK_TOKEN=$(grep '^token'  ~/.config/nook/auth.toml | sed 's/.*"\(.*\)"/\1/')
curl -s -X POST "$NOOK_SERVER/api/v1/tasks/KEY/move" \
  -H "Authorization: Bearer $NOOK_TOKEN" -H 'Content-Type: application/json' \
  -d '{"column":"In Progress"}'
```

Then check out its branch and fix only the "Must fix before merge" items, run
the relevant checks, remove `loop-changes-requested`, and comment with what
changed. Submitting the fix parks it back in In Review. End this pass.

**The repair follows §5's commit rules exactly** — it is the same branch, so it
keeps the same shape:

- **Amend the single commit.** Do not append a "fix review feedback" commit; the
  branch still carries one commit whose subject is the PR title.
- **Rebase** if the default branch has moved. Never merge it in.
- **`git push --force-with-lease`.** Never a bare `--force`.

If a proposed fix would cross an issue non-goal or requires a product decision,
do not implement it. Comment the exact conflict on both the PR and the issue,
add `needs-human-review` to the PR, remove `loop-changes-requested`, and end
the pass. This prevents the next loop iteration from retrying a decision only
a human can make.

## 2. Pick

One query does the whole pick:

```bash
nook tasks --label agent-ready --not-label blocked --assignee none --unblocked --this-node --json
```

Each flag is load-bearing: `--label agent-ready` is the human approval gate,
`--not-label blocked` skips issues waiting on a human answer, `--assignee none`
skips work someone already holds, `--unblocked` drops anything with an
unfinished blocker, and `--this-node` is how a DISPATCHED card finds you.

**What `--this-node` means.** A human can dispatch a card to a specific machine
— "this one wants the big box", "only that node has the runtime". With the flag
you see cards dispatched to the machine you are running on, PLUS everything
undispatched; you never see a card dispatched to somebody else. Without it, a
builder would take work meant for another node, which is the whole reason
dispatch used to do nothing useful.

So a dispatch is a HINT you honour, not an assignment you must wait for: an
undispatched card is still fair game for whoever gets there first, and the claim
in step 3 remains the thing that actually settles who does it.

On a machine that has not joined a fleet the flag is a no-op and says so — you
still see the undispatched work.

Results already arrive in the order work should be taken: urgent first, tasks
with **no** priority last, then oldest first. Take the first row. If the list
is empty, say so and end the pass. Do not invent work.

**The backlog and epics are excluded server-side** (MAIN-80). The pick query
above never returns a task in a `backlog`-type column (Triage) — the backlog is
a human refinement space the loop draws from only after a human sends a card to
the board — nor a `type='epic'` task (a tracker that decomposes into buildable
children, with no PR of its own). This holds regardless of labels, so an
`agent-ready` card left in Triage, or an approved-by-mistake epic, simply does
not appear. The command is unchanged; the server does the filtering. (`nook
tasks --backlog` includes backlog tasks for a human; a builder never uses it.
`type=epic` on the filter surfaces epics on purpose.) The four other types
(`task`/`bug`/`story`/`chore`) are all buildable. If you ever file or update a
task yourself, set an appropriate `type` (never `epic` for a unit of work you
intend to build).

Add `--board KEY` if the tenant has more than one board and this loop owns one
of them.

**You are confined to your workspace.** Run inside a **workspace** session (not
an ad-hoc terminal): `nook tasks` then scopes to that session's workspace
automatically — you only see, and can only take, tickets for the repo you are
in. A ticket for another workspace is invisible to the pick, and `nook claim`
refuses it outright even if you name its key, so you can never build another
repo's feature by mistake. `nook workspace current` shows which workspace you
are in; if it prints nothing you are not in a workspace session and must not run
the loop. (`--all-workspaces` and `--any-workspace` exist for humans; a builder
never uses them.)

## 3. Claim (the atomic lock)

```bash
nook claim KEY --column-type started
```

**`KEY` throughout this document is a placeholder, never a literal to copy.**
It stands for the **verbatim `key` of the ticket you claimed here** — read it
from the pick in step 2, or from `nook task <key> --json`. Boards do not all use
the same prefix: a second board's keys may be `ACME-7`, and a builder that
copies an example prefix produces a PR naming a ticket that does not exist.
Every `KEY`, `key-nn-slug` and `MAIN-42` below is that same placeholder.

Claim before reading deeply or writing code. The claim is atomic in the
database, so two builders polling the same queue cannot both win.

**A lost claim is normal, not an error.** If it reports the task was already
taken, go back to step 2 and take the next one. Never retry the same task —
an agent that retries the one task it cannot have will spin forever.

The server also refuses two claims outright (MAIN-80), each a distinct message,
not the lost-claim 409: **"task is in the backlog — send it to the board first"**
(a card still in Triage) and **"epics are containers and cannot be claimed"**.
Both mean *never claimable by the loop* — do not retry either; take the next
task. They should never reach you anyway, because the pick query already
excludes backlog and epic tasks; a claim that hits one means the row moved or
was hand-fed, and the answer is the same: move on.

Target the column *type* (`started`), never a column name. A human renaming
"In Progress" to "Doing" must not break this.

## 4. Read

```bash
nook task KEY
```

That returns the whole issue: description, labels, comments, blockers.
Implement only its acceptance criteria. Non-goals are binding. Compare every
`AC-N` against every `NG-N` before editing. No unrelated changes and no
opportunistic refactors.

If an acceptance criterion is ambiguous, conflicts with a non-goal, or depends
on an unresolved blocker, go to step 8. Never guess.

## 5. Build

- Fetch the latest default branch from `origin` and create or resume a branch
  named from the real key, **lowercased**, plus a short slug:
  `MAIN-42` → `main-42-short-slug`, `ACME-7` → `acme-7-short-slug`.
- Implement the acceptance criteria using the repository's existing style,
  architecture, and naming.
- Add or update tests when the change affects logic, data flow, permissions,
  integrations, or user-visible behavior.
- Preserve behavior outside the issue contract.

**One atomic commit per branch.** Its subject is byte-identical to the PR title
from step 7 — same sentence, same key, same trailing period. Further work on the
ticket **amends** that commit; it does not stack a second one. A branch whose
history is "do the thing" then "fix review feedback" then "fmt" tells a reviewer
nothing the diff does not already say, and it makes the PR title and the commit
subject disagree.

**Bring in the default branch by REBASE only.** Never merge it into the PR
branch: a merge commit in a one-commit branch is the one shape that cannot be
amended, and it puts changes in the diff that the PR did not author.

**Update a pushed branch with `git push --force-with-lease`.** Never a bare
`--force` — with-lease refuses when someone else has pushed since you last
fetched, which is exactly the case where overwriting is destructive.

## 6. Verify

Run the project's relevant lint, typecheck, build, and narrowest useful tests.
All checks attributable to this change must pass before opening a PR. If a
broad check has a pre-existing unrelated failure, run the relevant targeted
check, preserve the evidence, and disclose both results in the PR.

Review `git diff` and `git status` before shipping. Stop if the diff contains
unrelated work or generated secrets.

## 7. Ship

Push and open a PR with `gh pr create`.

**The title is `<Imperative present-tense sentence> (KEY).`** — a complete
English sentence, capitalized, imperative mood, present tense, with the real key
in parentheses and the period **after** the closing parenthesis:

```
Add a session navigator (MAIN-42).
```

Not `session navigator`, not `MAIN-42: add a session navigator`, not
`Adds a session navigator (MAIN-42)`. The reviewer checks this shape and a
non-conforming title comes back as a must-fix.

Its description must include:

- What changed and why
- **`Closes KEY` on its own line**, using the real key. This is the reviewer's
  ONLY join from the PR to its contract — it parses that literal text and
  nothing else. **Omit it and the PR stops dead**: the reviewer cannot read the
  issue, cannot check a single acceptance criterion, and escalates to
  `needs-human-review`, where it waits for a person for a reason that has
  nothing to do with the code in it.
- A scope ledger: one evidence line per `AC-N`, one preservation line per
  `NG-N`, and `Other behavior changes: None`
- Numbered manual test steps matching what was actually built
- Automated checks run and their results
- Risk: Low / Medium / High

If `Other behavior changes: None` is not true, stop and get the issue amended
before opening the PR.

Record the PR on the issue, then park the card in **In Review** — its home
while a human reviews and merges (Done means merged, so the builder never puts
a card there):

```bash
nook comment KEY "PR opened: <url>"
NOOK_SERVER=$(grep '^server' ~/.config/nook/auth.toml | sed 's/.*"\(.*\)"/\1/')
NOOK_TOKEN=$(grep '^token'  ~/.config/nook/auth.toml | sed 's/.*"\(.*\)"/\1/')
curl -s -X POST "$NOOK_SERVER/api/v1/tasks/KEY/move" \
  -H "Authorization: Bearer $NOOK_TOKEN" -H 'Content-Type: application/json' \
  -d '{"column":"In Review"}'
```

Never merge and never enable auto-merge. End the pass.

## 8. Blocked

Comment one specific question a human can answer asynchronously, then hand the
work back:

```bash
nook comment KEY 'Blocked: the fixture DB has no migrations. Add one, or point the test at the dev DB? Affects AC-2.'
nook label KEY blocked
```

Then release the claim so the issue is pickable again once a human answers.

> **Gap:** there is no `nook release` verb yet, though the API has the
> endpoint. Until the CLI catches up:
>
> ```bash
> NOOK_SERVER=$(grep '^server' ~/.config/nook/auth.toml | sed 's/.*"\(.*\)"/\1/')
> NOOK_TOKEN=$(grep '^token'  ~/.config/nook/auth.toml | sed 's/.*"\(.*\)"/\1/')
> curl -s -X POST "$NOOK_SERVER/api/v1/tasks/KEY/release" \
>   -H "Authorization: Bearer $NOOK_TOKEN"
> ```

Leave `agent-ready` in place: the pick query excludes `blocked`, so the issue
safely reappears only after a human answers and removes that label.

Never use "this is unclear" as the question. State the exact decision, the
available options, and which acceptance criterion it affects. End the pass so
the next iteration can pick different work.

## Hard rule

Never apply `agent-ready` to anything. It is the human's signal that an agent
may take a task; applying it yourself is approving your own work. Removing it
is fine — handing work back never needs approval.

