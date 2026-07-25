-- Board automation (MAIN-73): per-board rules that fire when a task enters a
-- column of a given TYPE. Stored as a jsonb map from column type to an ordered
-- action list, e.g.
--   {"review":[{"kind":"notify"}],
--    "completed":[{"kind":"remove_board_label","label":"agent-ready"}]}
-- Server-side validation (services/triggers.rs) rejects unknown kinds and
-- malformed config on write, so a stored config is always runnable.
--
-- Mirrors the existing `provider_config jsonb DEFAULT '{}'` on this table.
-- Append-only and idempotent.

ALTER TABLE public.boards
    ADD COLUMN IF NOT EXISTS automation jsonb NOT NULL DEFAULT '{}'::jsonb;
