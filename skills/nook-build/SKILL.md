---
name: nook-build
description: "Build the one NookOS card a run is directed at, end to end: read the contract, implement it in a branch, verify, open a PR, and report the outcome. Judgment only — the control plane picks, claims, moves cards and records. Designed for directed build runs; never merges."
version: 2.6.0
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
nook issues attachments "$NOOK_BUILD_TASK"        # address, type, size, id
nook issues download <ADDR>                       # into the working directory
nook issues download <ADDR> --out /tmp/spec.md
```

`<ADDR>` is what the listing prints — `MAIN-42/spec.md`, the card and the
filename — or the attachment's uuid.

Judgment, not a ritual: a mockup a criterion refers to is worth reading, a 12 MB
video is not. Nothing downloads by itself, so a run only pays for what it asked
for. `download` refuses to overwrite an existing file — pass `--out` to put it
somewhere else. Fetched files are scratch: keep them out of the commit unless a
criterion says otherwise.

If an acceptance criterion is ambiguous, conflicts with a non-goal, or depends
on something unresolved, do not guess — hand the decision to a human (§6,
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
title from §6 — same sentence, same key, same trailing period. Further work on
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

## 5. Show what you built

A run that produced something a person can drive ends by driving it: a video of
the feature working, on the card. **The judgment is yours, it is made from your
own diff, and it never blocks** (AC-2) — decide, write one line in the PR body
saying which way you went, and carry on. If you cannot tell, pick one and
continue. There is no question to ask and nothing to escalate here, and nothing
in this section can fail a run, hold a PR, or gate a merge.

**Decide: is there a flow worth watching?** The bar is a flow somebody would
actually sit through — a page they open, a form they submit, a list that changes
in front of them. Touching a frontend file is not the bar: a copy fix, a colour,
a type-only change, a backend-only diff all skip. Either way the PR body carries
one line, so the call is auditable (AC-1):

```
Walkthrough: recorded — the session navigator, at the `web` target.
Walkthrough: skipped — backend only; the diff adds nothing a person can drive.
```

**Find the target, never a literal.** The workspace declares which of its
listeners serve a UI; one command answers that and joins it to the ports this
run leased:

```bash
nook ports list --browsable --json     # name, env, path, port, url
```

Open the `url` it gives you. Do not read `NOOK_WEB_PORT`, or any other variable,
by name — which variable carries the UI is the workspace's choice, and a repo
serving its app on `ADMIN_PORT` is the case the hardcoding gets wrong (AC-3).

- **Several targets**: record the one your change affects and name it in the
  decision line. Cannot tell which? Pick one and continue (AC-3b).
- **No target**: record nothing and say `no browsable target declared` — that is
  a gap in the declaration, not a failure of this run.
- **One video per run**, whatever the repo's frontend count (NG-8).

**Bring the app up** the way its own docs say, in the checkout you built in. The
leased ports are already in your environment, so the app binds them by starting
normally — nothing here needs a special mode.

**Record the flow.** Playwright and headless Chromium ship in the executor image
(`nook-browser-check` proves it in one command). Drive the flow from a scratch
script **outside the repo** — `/tmp`, never the worktree — with video recording
on the context:

```js
// /tmp/walkthrough.js — plain CommonJS, run with `node /tmp/walkthrough.js`.
// Playwright is a GLOBAL install in the image, which node resolves only from
// `npm root -g`; the async wrapper is what keeps this a CJS file (a top-level
// `await` beside `require` parses as ESM and dies on the first line).
const { execFileSync } = require('node:child_process');
const root = execFileSync('npm', ['root', '-g'], { encoding: 'utf8' }).trim();
const { chromium } = require(`${root}/playwright`);

(async () => {
  const browser = await chromium.launch();
  const ctx = await browser.newContext({
    viewport: { width: 1280, height: 720 },
    recordVideo: { dir: '/tmp/walkthrough', size: { width: 1280, height: 720 } },
  });
  const page = await ctx.newPage();
  // …drive the feature, and ASSERT what the card promised…
  await ctx.close();   // the .webm is written HERE, not before
  await browser.close();
})().catch((err) => {
  // A non-zero exit is how the flow says it did not pass — read it, because
  // AC-4 turns on it: a failed assertion must leave no video behind.
  console.error(err);
  process.exit(1);
});
```

Keep it to the flow itself — half a minute, one viewport, no tour of the app.
That is how the file stays under the server's user-content cap (AC-9); there is
no compression step to fall back on.

**Assert, do not merely click.** Wait for the thing the acceptance criteria
promised and check it is there. A recording of a broken feature clicked through
in silence is worse than no recording.

**A failing walkthrough is ordinary build work** (AC-7). It is a defect you just
found in your own output, so fix it and run the flow again, exactly as for any
red test. It is not an escalation and not a reason to skip.

**Attach it only when the run passed end to end** (AC-4) — a partial run, an
aborted run or a failed assertion produces no video:

```bash
nook issues attach "$NOOK_BUILD_TASK" /tmp/walkthrough/<file>.webm --replace
```

`--replace` is the whole of "one video per card": a repair pass replaces the
video its earlier pass left rather than stacking a second one (AC-5).

**Every harness failure is silent** (AC-6). No browser in the image, no display,
a launch that crashes, an upload refused, a file over the cap: note the reason in
the decision line and open the PR anyway.

```
Walkthrough: skipped — chromium did not launch (`nook-browser-check` fails here).
```

This step cannot become a reason a PR does not exist.

**None of it is committed.** The script is scratch and the video is the artifact
— nothing runs on a future head (NG-2). A Playwright spec joins the repo only
when the card asks for one, and then it is ordinary work with ordinary tests
(AC-8).

## 6. Conclude — the structured ending

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
- The walkthrough line from §5 — recorded or skipped, and why
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
