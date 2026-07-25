-- Managed content store (MAIN-78): the control plane is the source of truth for
-- the managed `nookos` skill(s) and the managed hook set. Each row carries the
-- content, its sha256, a monotonic `version`, and when it last changed. Seeded
-- on boot from the binary's embedded defaults (see routes/managed.rs) — the
-- chain root the push protocol, matrix endpoints, UI and editor build on.
--
-- `default_sha256` records the shipped default a row was last seeded/refreshed
-- from, so a re-seed can tell "the shipped default changed" (refresh + bump
-- version) from "an operator edited this row" (leave it alone). A redeploy of
-- the SAME binary is therefore a no-op that never clobbers an operator edit
-- (sub-ticket 5), while a deploy carrying a newer default updates the row.
--
-- Append-only and idempotent.

CREATE TABLE IF NOT EXISTS public.managed_content (
    id             uuid PRIMARY KEY,
    -- 'skill' | 'hooks'
    kind           text NOT NULL,
    -- the skill name, or 'default' for the single hook set
    name           text NOT NULL,
    content        text NOT NULL,
    -- sha256 of `content` (which an operator may later edit, sub-ticket 5)
    sha256         text NOT NULL,
    version        bigint NOT NULL DEFAULT 1,
    -- sha256 of the shipped default this row was last seeded/refreshed from
    default_sha256 text NOT NULL,
    updated_at     timestamptz NOT NULL DEFAULT now(),
    UNIQUE (kind, name)
);
