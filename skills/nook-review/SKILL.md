---
name: nook-review
description: "Review open PRs against their linked NookOS board issue and required GitHub checks, then post a three-group verdict with loop labels. Use when asked to run the loop's reviewer or review its PR queue. Designed for /loop; never merges or pushes code."
version: 1.2.0
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

`nook whoami` must show a **workspace**, and `gh auth status` must pass. If
either fails, end the pass and say which one.

The workspace line is the real check, because it is what confines this pass to
one repo. Two identities satisfy it:

- a **user token** (`nook login --token nook_user_…`) — a person, tenant-wide;
- a **node token inside a managed session**, which the control plane scopes to
  that session's tenant and workspace.

The second is not a downgrade: a session-scoped node token reaches ONE
workspace, where a user token reaches the whole tenant. What used to be true —
"a node token cannot drive the board" — stopped being true when scope started
coming from the session rather than the credential. What still disqualifies a
run is `whoami` reporting **no workspace**: unconfined means the pick could
return another repo's cards.

## 1. Find a PR needing review

```bash
gh pr list --state open --json number,title,labels,isDraft,headRefOid,updatedAt,url
```

Skip drafts. For each PR, find the latest comment whose first line is
`Loop review of COMMIT_SHA`.

Skip a PR when that recorded SHA equals its current `headRefOid` and it already
has `loop-approved`, `loop-changes-requested`, or `needs-human-review`. Review
it again when new commits landed after the recorded SHA. If nothing needs
review, say so and end the pass.

### Your shard

A repo may run several reviewers at once. `NOOK_REVIEW_SHARDS` says how many,
and `NOOK_REVIEW_SHARD` says which one you are, counting from zero.

**When `NOOK_REVIEW_SHARDS` is greater than 1, consider only PRs where
`number % NOOK_REVIEW_SHARDS == NOOK_REVIEW_SHARD`.** That rule is about the
set of open PRs needing review — *however that set is obtained*. It is stated
that way on purpose: what lists the PRs may change, and this filter must
survive the change untouched.

Absent, empty, or `1` means every PR is yours. That is the case for a single
reviewer and for every deployment that has never set these, so the ordinary
run is unaffected.

The arithmetic is the whole coordination mechanism — there is no claim, no
lock, and no message between reviewers. Two shards therefore never pick the
same PR, and every PR belongs to exactly one shard. If your shard's queue is
empty, end the pass; do NOT take another shard's PR because you have nothing
to do. A shard whose reviewer is down leaves its PRs until it comes back, and
that is the accepted trade.

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

Non-goals are binding. If fixing a finding would require behavior excluded by
an `NG-N`, do not prescribe code. Record
`[SCOPE-CONFLICT AC-N ↔ NG-N]` with the exact contradiction and mark the PR for
human escalation.

## 3. Check merge evidence

Inspect the current PR head, mergeability, and required checks:

```bash
gh pr view NUMBER --json headRefOid,mergeable,mergeStateStatus
gh pr checks NUMBER --required --json bucket,name,state,link
```

- If required checks are pending or mergeability is still unknown, report that
  the PR is waiting and end without posting a verdict or changing labels. A
  later loop pass will retry it.
- Failed required checks are `[CI]` must-fix findings.
- A merge conflict is a `[DEFECT]` must-fix finding.
- If the repository has no required checks, mark the PR for human escalation;
  do not apply `loop-approved`. Missing CI is not green.

Review the exact `headRefOid` used for this evidence. Re-fetch it immediately
before posting. If it changed, discard the review and start again on a future
pass.

## 4. Post one verdict

Post one comment on the **PR** in this structure:

```md
Loop review of COMMIT_SHA

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
```

Mirror the verdict onto the board issue so the decision is durable where the
contract lives:

```bash
nook comment <KEY> 'Loop review of COMMIT_SHA — <verdict line>: <pr url>'
```

Then set GitHub labels based on the verdict, checking existing labels before
removing them so an absent label does not fail the command:

- No must-fix and no new escalation: add `loop-approved`; remove
  `loop-changes-requested`. Preserve a pre-existing `needs-human-review` label
  because it may represent a separate high-risk human gate.
- Must-fix present: add `loop-changes-requested`; remove `loop-approved`.
- Scope conflict or no required CI: add `needs-human-review`; remove both
  `loop-approved` and `loop-changes-requested`; set "Safe to merge" to
  `No — human decision required.`

Then put the same three names on the **board card**. That is what lets NookOS
answer "does this PR need repair?" without reaching for GitHub: the control
plane holds no GitHub credentials, and this one mirrored fact is what keeps the
build loop from needing any.

The board resolves a label by name and returns `404` for one that is not in the
tenant's vocabulary yet — on removals as well as adds, because both go through
the same lookup. So ensure the three exist first. `POST /api/v1/labels` returns
the existing row instead of erroring, which makes this safe to re-run every
pass:

```bash
NOOK_SERVER=$(grep '^server' ~/.config/nook/auth.toml | sed 's/.*"\(.*\)"/\1/')
NOOK_TOKEN=$(grep '^token'  ~/.config/nook/auth.toml | sed 's/.*"\(.*\)"/\1/')
for l in loop-approved loop-changes-requested needs-human-review; do
  curl -s -X POST "$NOOK_SERVER/api/v1/labels" \
    -H "Authorization: Bearer $NOOK_TOKEN" -H 'Content-Type: application/json' \
    -d "{\"name\":\"$l\"}" -o /dev/null
done
```

Then mirror whichever branch you just took, with the same adds and the same
removals:

```bash
nook label <KEY> loop-approved                    # the verdict you set
nook label <KEY> loop-changes-requested --remove  # each one that branch removes
```

Once the names exist, attach and detach are both idempotent — re-applying a
verdict the card already carries, or removing one it does not, succeeds. The
check-before-removing above is a `gh` workaround and is not needed here.

The preservation rule crosses over intact: a pre-existing `needs-human-review`
survives an otherwise-clean pass on the card exactly as it does on the PR, so a
card can legitimately carry it alongside `loop-approved`. Only the escalation
branch removes the other two outright.

**Mirroring is best-effort.** If a call fails, say so in the pass output and
carry on — the verdict comment is the record of truth, and a review that has
already posted must not be failed by a label write. Best-effort is not silent:
report it, because a mirror that quietly never ran would leave the build loop
reading a card that looks unreviewed.

The escalation path deliberately leaves the automated repair queue. A human
must resolve the reason, change the issue or repository configuration as
needed, and remove `needs-human-review` before the loop reviews that unchanged
commit again.

## 5. Hard limits

- Never merge or enable auto-merge.
- Never push commits to the PR branch.
- Never approve or request changes through a formal GitHub review. Use one
  comment plus labels because the loop may run on the PR author's token and
  GitHub rejects self-reviews.
- Never apply `agent-ready` on the board. A reviewer that could mark work
  ready would be approving the queue it feeds.
- `loop-approved` is evidence for a human, not merge authorization.

