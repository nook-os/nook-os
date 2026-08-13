---
name: nook-review
description: "Review open PRs against their linked NookOS board issue and required GitHub checks, then post a three-group verdict with loop labels. Use when asked to run the loop's reviewer or review its PR queue. Designed for /loop; never merges or pushes code."
version: 1.7.0
author: NookOS
license: MIT
platforms: [linux, macos]
metadata:
  hermes:
    tags: [NookOS, Board, Kanban, Review, Pull-Request, CI, Loop]
    category: autonomous-ai-agents
    related_skills: [nookos, nook-spec, nook-build, nook-epic]
---

# Loop reviewer

One pass = one PR reviewed. Under `/loop`, each iteration runs this skill once.

The contract lives on the NookOS board. The code, CI and verdict live on
GitHub. A PR under review has its card parked in the board's **In Review**
column (the builder moves it there on submit); **Done** means merged, so a card
only reaches Done when a human merges. This skill reads and labels; it never
moves cards and never merges.

## 0. Preflight

Every pass is DIRECTED: `NOOK_REVIEW_PR` names the one pull request this pass
is about, set by the control plane that raised the run. If it is not set,
something started this skill outside a review run — end the pass and say so;
there is no queue to scan and picking one would be inventing work. (The manual
path, `nook reviews enqueue`, raises directed runs too.)

The directive IS the confinement: one PR, in the repo this run was placed in.
Do not end a pass over `nook whoami`'s workspace line.

`gh auth status` must pass — the run provides the fleet credential as
`GH_TOKEN`; never improvise one from other variables. If it fails, end the
pass with `nook reviews verdict skipped --body -` explaining why, so the
failure is recorded rather than read as a review.

## 1. Your pull request

Read PR `$NOOK_REVIEW_PR`:

```bash
gh pr view "$NOOK_REVIEW_PR" --json number,title,body,labels,isDraft,headRefOid,url
```

The control plane already deduplicates runs and paces re-review; your only
skip-check is against work that reached GitHub without it noticing. Find the
latest comment whose first line is `Loop review of COMMIT_SHA`. If that SHA
equals the current `headRefOid` — or the PR is a draft — there is nothing new
to conclude: record it and end the pass:

```bash
nook reviews verdict skipped --body "already reviewed at $HEAD" 
```

**Unless `NOOK_REVIEW_FORCED` is set (MAIN-473).** A human forced this run
precisely because the head is unchanged and the existing verdict's evidence
went stale — a CI rerun turned green, a judgment was overtaken. Skip only the
draft check and review the current head in full; your verdict supersedes the
recorded one. Never set this yourself: it arrives only from a forced enqueue.

Never touch another PR because yours needed nothing.

## 2. Read the contract and code

- Parse the issue key from the `Closes <KEY>` line in the PR body and read the
  whole issue:

  ```bash
  nook task <KEY>
  ```

  **`<KEY>` is whatever that line names — do not assume a prefix.** Boards do
  not share one: a second board's keys may be `ACME-7`. Take the key verbatim
  from the PR body rather than matching a shape you expect.

  That returns the description, labels, comments, blockers, and the issue
  `type` (`task`/`bug`/`epic`/`story`/`chore`).

  **No `Closes` line at all → `needs-human-review`, unchanged.** This is
  deliberate and stays that way: with no key there is no contract, so there is
  nothing to review against, and guessing one — from the branch name, the title,
  or the diff — would produce a verdict about the wrong ticket. A later reader
  should not "improve" this into a silent inference. Escalate and stop.

- **The PR title must read `<Imperative present-tense sentence> (KEY).`** — a
  complete sentence, capitalized, imperative, present tense, key in parentheses,
  period after the parenthesis: `Add a session navigator (MAIN-42).` A title
  that does not match is a `[DEFECT]` must-fix, so it comes back through
  `loop-changes-requested` and the builder repairs it.

  **Check this only when the ticket resolved** — i.e. `Closes` was present and
  `nook task` returned the issue. A PR with no key is already going to
  `needs-human-review`, and adding a title finding on top would just be noise on
  a PR nobody is going to repair automatically.
- **Account for the type.** An `epic` is a tracker/roadmap parent, not a unit of
  work — it decomposes into buildable children and should have no PR closing it.
  A PR whose `Closes` points at an `epic` is either closing a tracker or built
  against a mis-typed ticket: treat it as a `[DEFECT]` and mark the PR for human
  escalation rather than approving it. For the four buildable types
  (`task`/`bug`/`story`/`chore`) review normally; a `bug` fix in particular
  should carry a regression test, and its absence is a must-fix `[AC-N]`/`[DEFECT]`
  when the issue's acceptance criteria imply one.
- Read the full diff and every changed file in context.
- Review only against the linked issue: acceptance-criteria gaps, defects,
  broken data flow, unnecessary scope expansion, security problems, missing
  loading/error states, and code future agents will struggle to modify.
- Do not suggest unrelated improvements unless they are severe.

Every must-fix code finding starts with one of:

- `[AC-N]` — the PR does not satisfy that acceptance criterion
- `[DEFECT]` — the implementation is broken while staying inside scope
- `[SECURITY]` — a severe security issue blocks shipping
- `[CI]` — a required GitHub check failed

Non-goals are binding, and `[SCOPE-CONFLICT AC-N ↔ NG-N]` records two shapes of
the same fault. Either way: name the exact pair, state the contradiction, do not
prescribe code, and mark the PR for human escalation.

- **The diff crosses a non-goal.** Fixing a finding would require behavior an
  `NG-N` excludes, so there is no fix to prescribe inside the contract.
- **The card contradicts itself.** An `AC-N` cannot be satisfied *at all*
  without violating an `NG-N` — the contradiction is in the spec, not in the
  diff. Record it **against the card**: say which pair is irreconcilable and
  that whichever side the PR took, the fault is upstream of it. Do not restate
  it as an `[AC-N]` or `[DEFECT]` finding; the builder cannot repair a
  contradiction, and asking it to would only swap which half of the contract
  the PR breaks. (`nook-spec` and `nook-epic` cross-check this before filing;
  what reaches you here is a card that predates that step or slipped past it.)

## 3. Check merge evidence

Inspect the current PR head, mergeability, and required checks:

```bash
gh pr view NUMBER --json headRefOid,mergeable,mergeStateStatus
gh pr checks NUMBER --required --json bucket,name,state,link
```

- If required checks are pending or mergeability is still unknown, say so and
  end the pass with NO verdict. The control plane holds a verdict-less pass
  and raises a fresh run after the hold — do not wait CI out yourself, and do
  not record `skipped`, which would mark this head reviewed when nothing was.
- Failed required checks are `[CI]` must-fix findings.
- A merge conflict is a `[DEFECT]` must-fix finding.
- If the repository has no required checks, mark the PR for human escalation;
  do not apply `loop-approved`. Missing CI is not green.

Review the exact `headRefOid` used for this evidence. Re-fetch it immediately
before posting. If it changed, discard the review and start again on a future
pass.

## 4. Conclude

Decide one verdict:

- `approved` — no must-fix and no new escalation.
- `changes_requested` — at least one must-fix.
- `needs_human` — a scope conflict, no required CI, or anything only a person
  can rule on.

Then report it — one call, and it is the pass's LAST act:

```bash
nook reviews verdict changes_requested --body - <<'MD'
CI: required checks passed | failed | not configured
Mergeability: clean | conflicting

## Review

Summary: one or two plain-language sentences on what this PR does.

## 1. Must fix before merge

None.

## 2. Should fix soon

None.

## 3. Safe to merge

Yes — automated review evidence is complete. A human still makes the merge decision.
MD
```

The control plane posts the `Loop review of COMMIT_SHA` comment and maintains
the verdict labels (`loop-approved` / `loop-changes-requested` /
`needs-human-review`) on the PR — never post the comment or edit PR labels
through `gh` yourself. If the call fails, the verdict did NOT land: say so and end the pass
as a failure, never post the comment by hand as a fallback.

The control plane also mirrors the verdict COMMENT onto the board card itself
(MAIN-477) — collapsed, so a redelivered conclusion never stacks duplicates —
through the `Closes <KEY>` join in the PR body. Do not `nook comment` the
verdict line yourself. What remains yours is the card's LABEL state:

```bash
nook label <KEY> <the verdict label>          # and --remove the one it replaces
```

Attach and detach are idempotent: re-applying a label the card already carries
changes nothing and records no event, so a retried pass is safe. A pre-existing
`needs-human-review` on the card survives an otherwise clean pass — it may be a
separate human gate. **Mirroring is best-effort**: if a board call fails, say so
and carry on — the posted verdict is the record of truth — but never silently,
or the build loop reads a card that looks unreviewed.

## 5. Hard limits

- Never merge or enable auto-merge.
- Never push commits to the PR branch.
- Never approve or request changes through a formal GitHub review, and never
  post the verdict comment or edit PR labels with `gh` — `nook reviews
  verdict` is the one delivery path, and the control plane does the posting.
- Never apply `agent-ready` on the board. A reviewer that could mark work
  ready would be approving the queue it feeds.
- `loop-approved` is evidence for a human, not merge authorization.

