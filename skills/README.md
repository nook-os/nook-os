# Agent skills

Skills that teach an AI agent how to use NookOS. They live in the repo so the
instructions and the CLI they describe version together — a skill that drifts
from its tool is worse than no skill, because the agent confidently does the
wrong thing.

| Skill | Teaches |
|---|---|
| [`nookos/SKILL.md`](nookos/SKILL.md) | Driving sessions on other machines with `nook`: start a Claude/Codex/bash session anywhere in the fleet, type into it, read the answer. No ssh, no tmux. |
| [`nook-spec/SKILL.md`](nook-spec/SKILL.md) | The spec interview: turn a raw idea into a build-ready NookOS board issue, by researching the code and interviewing the user in rounds. Run with `/nook-spec`. |
| [`nook-build/SKILL.md`](nook-build/SKILL.md) | The builder: build (or repair) the one card a run is directed at — `NOOK_BUILD_TASK` — and report how it ended with `nook builds outcome`. Judgment only: the control plane picks, claims, moves cards and records. One run, one card; never merges. |
| [`nook-review/SKILL.md`](nook-review/SKILL.md) | The reviewer: check an open PR against its linked issue and required CI, then post a three-group verdict and set the loop labels. Run with `/nook-review`; never merges. |
| [`nook-epic/SKILL.md`](nook-epic/SKILL.md) | The epic decomposer: walk opted-in (`auto-spec`) epics and file the next ready sub-ticket from the epic's own body, in the `/nook-spec` shape, into the backlog. Run with `/nook-epic`; unattended, one ticket per pass, escalates instead of guessing. |
| [`nook-investigate/SKILL.md`](nook-investigate/SKILL.md) | The investigator: explain the support report a card was filed from, READ-ONLY — read the sealed original with `nook emails read`, reproduce, and record findings plus a draft reply with `nook emails record`. Writes no code, opens no PR, never promotes the card. |
| [`nook-yolo/SKILL.md`](nook-yolo/SKILL.md) | The board-wide overnight merge pass: land every loop-approved PR whose evidence clears, across the whole workspace, unattended. Run with `/loop 20m /nook-yolo`. Writes no code and never pushes; where the epic runner halts on trouble, this skips the PR, records why in the day's ledger, and keeps going. |

## Format

`SKILL.md` with YAML frontmatter (`name`, `description`, `version`,
`platforms`, `metadata.hermes.tags`). This is the Hermes skill layout, and it
reads fine as plain Markdown for any other agent — Claude Code, Codex, or a
human.

## Installing

For Hermes, skills are directories under `~/.hermes/skills/<category>/<name>/`,
and each agent profile keeps its own copy under
`~/.hermes/profiles/<profile>/skills/`. `nook skills install` puts the skill in the
shared location and in every profile:

```bash
nook skills install            # every agent found on this machine
nook skills install --dir DIR  # somewhere specific
```

For any other agent, point it at the file — there's nothing machine-specific in
it beyond the control-plane hostname in the examples.

## Keeping it honest

Every command and error message in `nookos/SKILL.md` was executed against a
live fleet, and the transcripts are pasted verbatim. When you change the CLI,
re-run the examples rather than editing them from memory: agents follow this
literally, and a stale flag is a silent failure.
