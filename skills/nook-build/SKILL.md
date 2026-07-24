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
    related_skills: [nookos, nook-spec, nook-review]
---

# Loop builder

One pass = one unit of work: fix review feedback on one existing PR, or build
one issue end to end. Under `/loop`, each iteration runs this skill once.

The board is NookOS. PRs and CI stay on GitHub.

## 0. Preflight

Before changing the board, GitHub, branches, or files:

- `nook whoami` must report a **user** token. A node token cannot drive the
  board. If it fails, end the pass and tell the user to
  `nook login --token nook_user_…`.
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
board issue (`nook task NOOK-NN`) and the latest `Loop review of COMMIT_SHA`
verdict in that issue's comments. Check out its branch, fix only the "Must fix
before merge" items, run the relevant checks, push, remove
`loop-changes-requested`, and comment with what changed. End this pass.

If a proposed fix would cross an issue non-goal or requires a product decision,
do not implement it. Comment the exact conflict on both the PR and the issue,
add `needs-human-review` to the PR, remove `loop-changes-requested`, and end
the pass. This prevents the next loop iteration from retrying a decision only
a human can make.

## 2. Pick

One query does the whole pick:

```bash
nook tasks --label agent-ready --not-label blocked --assignee none --unblocked --json
```

Each flag is load-bearing: `--label agent-ready` is the human approval gate,
`--not-label blocked` skips issues waiting on a human answer, `--assignee none`
skips work someone already holds, and `--unblocked` drops anything with an
unfinished blocker.

Results already arrive in the order work should be taken: urgent first, tasks
with **no** priority last, then oldest first. Take the first row. If the list
is empty, say so and end the pass. Do not invent work.

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
nook claim NOOK-42 --column-type started
```

Claim before reading deeply or writing code. The claim is atomic in the
database, so two builders polling the same queue cannot both win.

**A lost claim is normal, not an error.** If it reports the task was already
taken, go back to step 2 and take the next one. Never retry the same task —
an agent that retries the one task it cannot have will spin forever.

Target the column *type* (`started`), never a column name. A human renaming
"In Progress" to "Doing" must not break this.

## 4. Read

```bash
nook task NOOK-42
```

That returns the whole issue: description, labels, comments, blockers.
Implement only its acceptance criteria. Non-goals are binding. Compare every
`AC-N` against every `NG-N` before editing. No unrelated changes and no
opportunistic refactors.

If an acceptance criterion is ambiguous, conflicts with a non-goal, or depends
on an unresolved blocker, go to step 8. Never guess.

## 5. Build

- Fetch the latest default branch from `origin` and create or resume a branch
  named `nook-42-short-slug`, using the issue's real key.
- Implement the acceptance criteria using the repository's existing style,
  architecture, and naming.
- Add or update tests when the change affects logic, data flow, permissions,
  integrations, or user-visible behavior.
- Preserve behavior outside the issue contract.

## 6. Verify

Run the project's relevant lint, typecheck, build, and narrowest useful tests.
All checks attributable to this change must pass before opening a PR. If a
broad check has a pre-existing unrelated failure, run the relevant targeted
check, preserve the evidence, and disclose both results in the PR.

Review `git diff` and `git status` before shipping. Stop if the diff contains
unrelated work or generated secrets.

## 7. Ship

Push and open a PR with `gh pr create`. Its description must include:

- What changed and why
- `Closes NOOK-42`, using the real board key. This is the only join between
  the PR and the issue — the review skill parses it.
- A scope ledger: one evidence line per `AC-N`, one preservation line per
  `NG-N`, and `Other behavior changes: None`
- Numbered manual test steps matching what was actually built
- Automated checks run and their results
- Risk: Low / Medium / High

If `Other behavior changes: None` is not true, stop and get the issue amended
before opening the PR.

Record the PR on the issue, then leave it in the started column for a human to
move on merge:

```bash
nook comment NOOK-42 "PR opened: <url>"
```

Never merge and never enable auto-merge. End the pass.

## 8. Blocked

Comment one specific question a human can answer asynchronously, then hand the
work back:

```bash
nook comment NOOK-42 'Blocked: the fixture DB has no migrations. Add one, or point the test at the dev DB? Affects AC-2.'
nook label NOOK-42 blocked
```

Then release the claim so the issue is pickable again once a human answers.

> **Gap:** there is no `nook release` verb yet, though the API has the
> endpoint. Until the CLI catches up:
>
> ```bash
> NOOK_SERVER=$(grep '^server' ~/.config/nook/auth.toml | sed 's/.*"\(.*\)"/\1/')
> NOOK_TOKEN=$(grep '^token'  ~/.config/nook/auth.toml | sed 's/.*"\(.*\)"/\1/')
> curl -s -X POST "$NOOK_SERVER/api/v1/tasks/NOOK-42/release" \
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

