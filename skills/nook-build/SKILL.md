---
name: nook-build
description: "Build the one NookOS card a run is directed at, end to end: read the contract, implement it in a branch, verify, open a PR, and report the outcome. Judgment only — the control plane picks, claims, moves cards and records. Designed for directed build runs; never merges."
version: 2.4.0
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

One run = one card, built end to end or handed back with a reason. The control
plane selects the card, claims it, moves it across the board and records what
happened; this skill holds only the judgment — reading the contract, writing
the code, deciding how the run ended.

The contract lives on the NookOS board. The code, the PR and CI live on GitHub.

## 0. Preflight

Every pass is DIRECTED: `NOOK_BUILD_TASK` names the one card this run is
about, set by the control plane that raised the run. If it is not set,
something started this skill outside a build run — end the pass and say so;
there is no queue to scan and picking a card would be inventing work. (The
manual path, `nook builds enqueue`, raises directed runs too.)

The directive IS the confinement: one card, in the repo this run was placed
in. The card was claimed by the control plane before this run existed — do not
claim it, move it, or label it.

`gh auth status` must pass — the run provides its credential; never improvise
one from other variables. If it fails, say so and end the pass as a FAILURE
with no outcome reported: the control plane holds a run that concluded nothing
and raises a fresh one after the hold. A broken environment is not `nothing`.

## 1. Your card

```bash
nook task "$NOOK_BUILD_TASK"
```

That returns the whole issue: description, labels, comments, blockers.
Implement only its acceptance criteria. Non-goals are binding. Compare every
`AC-N` against every `NG-N` before editing, and read the comments — a human
ruling in a comment amends the contract. No unrelated changes and no
opportunistic refactors.

**A card can carry files, and they are part of the brief.** When `nook task`
prints an Attachments section, a document, a screenshot or a schema was hung on
the ticket or on one of its comments — it is contract, not decoration, and
implementing without reading it is implementing from an incomplete brief. Fetch
what the description depends on and nothing else:

```bash
nook attachments list "$NOOK_BUILD_TASK"   # filename, type, size, id
nook attachments get <ID>                  # into the working directory
nook attachments get <ID> --out /tmp/spec.md
```

Judgment, not a ritual: a mockup a criterion refers to is worth reading, a 12 MB
video is not. Nothing downloads by itself, so a run only pays for what it asked
for. `get` refuses to overwrite an existing file — pass `--out` to put it
somewhere else. Fetched files are scratch: keep them out of the commit unless a
criterion says otherwise.

If an acceptance criterion is ambiguous, conflicts with a non-goal, or depends
on something unresolved, do not guess — hand the decision to a human (§5,
blocked, in either of its two shapes).

If the card already records a PR, this run is a repair, not a rebuild — §2.

## 2. Repair — when the card already records a PR

A card whose `nook task` output shows a **`pr:` line** is not new work: a PR
exists, and this run exists to REPAIR it. The control plane raises these runs
when the PR carries `loop-changes-requested` (a reviewer's verdict, or the
hygiene pass routing a rebase here — for a conflict, or for an ejection from
the merge queue).

- The run's contract is on the PR, under one of three comment markers:
  - **`Loop review of COMMIT_SHA`** — a reviewer's verdict. Fix **only** its
    "Must fix before merge" items. Should-fix items are welcome only when
    they do not widen the diff's scope.
  - **`Loop conflict check of <head>`** — the conflict hygiene pass. The
    contract is exactly a rebase: bring in the default branch by rebase,
    resolve, and re-verify. No verdict findings to work through.
  - **`Loop queue ejection of <head>`** — the merge queue threw this PR out
    because the build of it MERGED INTO the current default branch failed.
    **This branch's own checks are green and re-running them proves nothing** —
    that build exists nowhere but the queue. The contract is: rebase onto the
    current default branch, find what the two changes do to each other, fix it,
    and make the checks pass ON THE REBASED BRANCH. Ending a pass because "the
    tests already pass" is the one wrong answer here. No verdict findings.
- When more than one is present, rebase first, then apply the newest verdict's
  must-fix list **only if that verdict names the branch's current head** — a
  verdict for an older head was already answered by the amends that moved it,
  and re-working it repeats finished work.
- The branch keeps its one-commit shape (§3): **amend** the existing commit,
  never stack a "fix review feedback" commit, and push with
  `git push --force-with-lease`.
- End the run with the SAME PR, never a new one:

  ```bash
  nook builds outcome pr --url <the existing PR's URL>
  ```

- If a demanded fix would cross the card's non-goals or needs a product
  decision, do not implement it — end with `nook builds outcome blocked
  --question -` stating the exact conflict and the AC it affects. The control
  plane and reviewer route the escalation from there; never edit PR labels
  yourself.

## 3. Build

- Fetch the latest default branch from `origin` (detect it with
  `gh repo view --json defaultBranchRef --jq .defaultBranchRef.name`; never
  assume `main`) and create or resume a branch named from the real key,
  **lowercased**, plus a short slug: `MAIN-42` → `main-42-short-slug`.
- Work in the checkout the run starts in: never relocate to a checkout other
  sessions may hold, and never switch a shared checkout's branch. On
  2026-08-08 two builders sharing one moved its branch under each other
  mid-build (MAIN-475).
- **Teardown is not the skill's job.** Do not `git worktree remove` at the end
  of a pass.
- Implement the acceptance criteria using the repository's existing style,
  architecture, and naming.
- Add or update tests when the change affects logic, data flow, permissions,
  integrations, or user-visible behavior.
- Preserve behavior outside the issue contract.

**One atomic commit per branch.** Its subject is byte-identical to the PR
title from §5 — same sentence, same key, same trailing period. Further work on
the card **amends** that commit; it does not stack a second one.

**Bring in the default branch by REBASE only.** Never merge it into the PR
branch: a merge commit in a one-commit branch is the one shape that cannot be
amended, and it puts changes in the diff that the PR did not author.

**Update a pushed branch with `git push --force-with-lease`.** Never a bare
`--force` — with-lease refuses when someone else has pushed since you last
fetched, which is exactly the case where overwriting is destructive.

## 4. Verify

Run the project's relevant lint, typecheck, build, and narrowest useful tests.
All checks attributable to this change must pass before opening a PR. If a
broad check has a pre-existing unrelated failure, run the relevant targeted
check, preserve the evidence, and disclose both results in the PR.

Review `git diff` and `git status` before shipping. Stop if the diff contains
unrelated work or generated secrets.

## 5. Conclude — the structured ending

Every run ends with exactly one outcome call, and it is the pass's LAST act.
The control plane records it, mirrors it to the board — the card's comment,
column and claim are its writes, not yours — and, for a PR, validates the
`Closes <KEY>` join. If the call fails, the outcome did NOT land: say so and
end the pass as a failure; never mirror the board by hand as a fallback.

**Opened a PR** — push and open it with `gh pr create`, then report it:

```bash
nook builds outcome pr --url <the PR's URL>
```

The title is `<Imperative present-tense sentence> (KEY).` — a complete
English sentence, capitalized, imperative mood, present tense, the real key in
parentheses, period after the closing parenthesis:

```
Add a session navigator (MAIN-42).
```

The description must include:

- What changed and why
- **`Closes KEY` on its own line**, using the real key. This is the reviewer's
  ONLY join from the PR to its contract, and the outcome call validates it —
  a PR whose body names the wrong card, or none, is refused.
- A scope ledger: one evidence line per `AC-N`, one preservation line per
  `NG-N`, and `Other behavior changes: None`
- Numbered manual test steps matching what was actually built
- Automated checks run and their results
- Risk: Low / Medium / High

If `Other behavior changes: None` is not true, stop and hand the card back
blocked instead of opening the PR.

**Blocked** — a decision only a human can make. Two shapes, by how soon the
answer can come:

- *Answerable now, worth waiting for*: ask through the durable interaction
  channel and keep the run alive — it pauses `waiting_on_human` and resumes on
  the answer:

  ```bash
  nook interactions ask --wait 'Exact question, with the options and the AC it affects.'
  ```

- *Async handback*: end the run and give the card back with the question on
  it — the control plane comments it, labels the card `blocked`, and releases
  the claim so a human's answer makes it pickable again:

  ```bash
  nook builds outcome blocked --question - <<'Q'
  Blocked: the fixture DB has no migrations. Add one, or point the test at
  the dev DB? Affects AC-2.
  Q
  ```

Never use "this is unclear" as the question. State the exact decision, the
available options, and which acceptance criterion it affects.

**Nothing to do** — the card is already satisfied at this content (the change
exists on the default branch, or the contract asks for what is already true):

```bash
nook builds outcome nothing
```

Nothing else counts as an ending. A run that opened a PR and did not report it
has told nobody — the board still shows the card in flight and the reviewer
never hears about the PR. That silent lie is what the outcome call ends.

## Hard limits

- Never merge and never enable auto-merge.
- Never apply `agent-ready` to anything. It is the human's signal that an
  agent may take a task; applying it yourself is approving your own work.
  Removing it is fine — handing work back never needs approval.
- Never claim, move, label or comment the card by hand for mechanics the
  outcome call performs — one delivery path, so the board cannot half-update.
