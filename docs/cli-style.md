# CLI style: the top level is frozen (MAIN-157)

`nook`'s top level held **thirty flat verbs** when this was written, and holds
**twenty-two** now. It got there one feature at a time — each ticket reasonably
adding one command, none of them wrong on its own, and nobody ever seeing the
total. `nook get sessions` and `nook interactions ask` show the shape it should
have had.

This document is the convention, and
`crates/nook-node/src/main.rs`'s `cli_surface` test is its enforcement.

## The rule

**New commands land as `nook <plural-noun> <verb>`.**

```
nook issues move MAIN-42 review           ✅  noun, then verb
nook interactions ask "…"                 ✅
nook move-issue MAIN-42 review            ❌  one more flat verb
```

Adding a top-level flat verb is prohibited. Adding a **noun group** is not —
that IS the convention, it needs no permission, and the enforcement test lets it
through untouched. The freeze is on leaves, never on groups.

### Naming

- **Plural nouns** for groups: `issues`, `sessions`, `workspaces`, `nodes` —
  not `issue`, not `session`. The group names a collection you act on.
  - **`notebook` is the one ruled exception** (MAIN-575, owner ruling
    2026-08-14). Singular, for two reasons worth having on the record: it
    matches the `/notebook` URL the web UI already uses, so one thing has one
    name across both surfaces; and the plural, `notes`, would collide with the
    unrelated tenant/workspace note system (MAIN-245/254) — two different
    resources answering to one word is worse than a singular noun. The
    exception is written down rather than absorbed: the next group that wants
    a singular name has to earn one the same way, in a ruling somebody can
    point at.
- **Verb second**, and an ordinary one: `list`, `get`, `create`, `move`, `ask`.
- **No new abbreviations.** `interactions`, not `ix`. The existing short ones
  (`exec`, `whoami`) are grandfathered; do not add siblings for them.
  - **The `attachments rm` exception is RETIRED** (MAIN-610, owner ruling
    2026-08-15). MAIN-594 AC-7 granted it, and folding the group into `issues`
    spelled the verb out — `nook issues detach` — so the thing it was granted
    for no longer exists. Retiring it rather than leaving it on the page is the
    point: an exception is earned by a ruling somebody can point at, and this
    one has stopped applying. The general rule stands unweakened: spell the
    verb out unless a ticket rules otherwise.
- Prefer an existing group over a new one. A new noun is justified when the
  thing it names is a real resource, not when it is a convenient prefix.
  - **`attachments` folding into `issues` is the worked example** (MAIN-610).
    An attachment is never free-standing — it hangs on a card or on one of that
    card's comments — so a group of its own was a prefix, not a resource, and
    its four verbs are `nook issues attach|attachments|download|detach` now.
    Note what it did NOT cost: the freeze is on flat leaves, never on groups,
    so moving a group's verbs under another group adds no flat verb.
    A retired group stays as a **hidden alias for one release**, printing the
    replacement, because a fleet is running the old spelling in skill text
    nobody has re-taught yet.
  - **The alias half of that precedent was NOT followed by MAIN-644**, which
    buried the eight flat board verbs here — `nook issues list|get|create|
    comment|label|claim|relate|set-description`. Owner ruling, 2026-08-19: the
    debt leaves in that ticket or it never leaves, and a hidden alias is the
    shape that never leaves. What the removal keeps from the precedent is the
    sentence: each retired spelling exits non-zero naming its replacement, so
    a fleet on old skill text fails with something to act on rather than with
    clap's list of the names it does know.

### Naming the thing a verb acts on

A group whose resource has both a **human address** and an id should accept
either, in every place it names one. `nook notebook` (MAIN-575) is the
exemplar: a note or folder is given as a slash-delimited path
(`"Nook/Ideas/2026-08-13"`) or as a uuid, and the path is matched against the
address the group's own `list` prints — so what a person copies out of the
output works verbatim, and a script that kept an id keeps working too.

`nook issues download MAIN-42/shot.png` (MAIN-610) follows it: the card and the
filename ARE the address, and `nook issues attachments` prints exactly that
string beside the uuid, so either can be pasted into the next command.

Two conventions the notebook group did not invent but that every new group
should follow, because a skill written against one CLI verb should not have to
learn a second dialect for the next:

- **`--content -` reads stdin**, and a lone `-` is never content itself
  (`nook issues set-description`, MAIN-470 AC-1).
- **`--json` on every verb** emits the API shape unchanged, and human-readable
  output is what you get without it.

## The grandfathered list — thirty, now twenty-two

The remaining flat verbs are frozen where they are, pending **MAIN-139**, which
buries them under nouns one at a time. They are not an example to follow — they
are the debt this rule exists to stop growing.

The ratchet, in one line: **additions refused, removals welcome.** The frozen
list in `cli_surface` only ever shrinks; every line deleted from it is MAIN-139
making progress. It is a high-water mark, not a target.

**MAIN-644 is the first pass, and the worked example of one.** Seven flat verbs
left the list — `claim`, `comment`, `label`, `relate`, `set-description`,
`task`, `tasks` — and the `create` group went with `create task`, its only
child. That is the shape a pass takes: one resource at a time, every verb of it,
no aliases and no half-move.

## The skills sweep — same ticket, never a follow-up

`skills/` ships prompts that drive this CLI, and they are taught to a fleet with
`nook teach`. A skill teaching a command that no longer exists does not fail at
review — it fails later, on a machine nobody has re-taught, inside an agent loop
with no human watching.

So **any ticket that adds, renames, or removes a CLI verb must, in that same
ticket**:

1. `grep -rn "nook <verb>" skills/` and update **every** hit.
2. Bump the `version:` in the frontmatter of each skill touched.
3. Say in the PR description that `nook teach` must be re-run for those skills.

Never as a follow-up card. The window between the rename landing and the sweep
landing is exactly the window in which the fleet is broken.

For scale: MAIN-644 moved eight board verbs and the sweep was 54 references
across eight skills. A rename is never a one-line change.

## Amending the freeze

The frozen list is meant to be amendable — deliberately, and visibly. If a flat
verb genuinely cannot live under a noun, edit `FROZEN_LEAVES` in the same PR
that adds it and say why in the description. The test's job is not to make that
impossible; it is to make it a decision somebody reviews rather than a drift
nobody notices.
