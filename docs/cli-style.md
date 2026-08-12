# CLI style: the top level is frozen (MAIN-157)

`nook`'s top level holds **thirty flat verbs**. It got there one feature at a
time — each ticket reasonably adding one command, none of them wrong on its own,
and nobody ever seeing the total. `nook get sessions` and `nook interactions
ask` show the shape it should have had.

This document is the convention, and
`crates/nook-node/src/main.rs`'s `cli_surface` test is its enforcement.

## The rule

**New commands land as `nook <plural-noun> <verb>`.**

```
nook issues move MAIN-42 review           ✅  noun, then verb
nook interactions ask "…"                 ✅
nook move-issue MAIN-42 review            ❌  a thirty-first flat verb
```

Adding a top-level flat verb is prohibited. Adding a **noun group** is not —
that IS the convention, it needs no permission, and the enforcement test lets it
through untouched. The freeze is on leaves, never on groups.

### Naming

- **Plural nouns** for groups: `issues`, `sessions`, `workspaces`, `nodes` —
  not `issue`, not `session`. The group names a collection you act on.
- **Verb second**, and an ordinary one: `list`, `get`, `create`, `move`, `ask`.
- **No new abbreviations.** `interactions`, not `ix`. The existing short ones
  (`exec`, `whoami`) are grandfathered; do not add siblings for them.
- Prefer an existing group over a new one. A new noun is justified when the
  thing it names is a real resource, not when it is a convenient prefix.

## The grandfathered thirty

The existing flat verbs are frozen where they are, pending **MAIN-139**, which
buries them under nouns one at a time. They are not an example to follow — they
are the debt this rule exists to stop growing.

The ratchet, in one line: **additions refused, removals welcome.** The frozen
list in `cli_surface` only ever shrinks; every line deleted from it is MAIN-139
making progress. It is a high-water mark, not a target.

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

For scale: the loop skills reference `nook tasks` and `nook comment` sixteen
times each, `nook start` and `nook notify` ten each. A rename is never a
one-line change.

## Amending the freeze

The frozen list is meant to be amendable — deliberately, and visibly. If a flat
verb genuinely cannot live under a noun, edit `FROZEN_LEAVES` in the same PR
that adds it and say why in the description. The test's job is not to make that
impossible; it is to make it a decision somebody reviews rather than a drift
nobody notices.
