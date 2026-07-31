-- MAIN-314: placement inputs. A node gains operator-set `labels` and `taints`;
-- the derived `os`/`arch` labels are computed from `platform` at read time and
-- deliberately NOT stored, so they cannot drift from the platform the node
-- actually reports.
--
-- Two columns rather than one: a label is an attribute a scheduler matches on,
-- a taint is a refusal the scheduler must tolerate. Merging them would make
-- "does this node have X" and "does this node refuse X" the same question.
--
-- Shapes, enforced at the route rather than here (a CHECK on jsonb structure
-- would reject a future field for no gain):
--   labels  {"key": "value", …}                      an object of strings
--   taints  [{"key": "no-linux", "effect": "NoSchedule"}, …]
--
-- Idempotent and additive: NOT NULL with safe empty defaults, no existing
-- column touched. An unlabelled, untainted node reads exactly as it did before.
ALTER TABLE public.nodes ADD COLUMN IF NOT EXISTS labels jsonb NOT NULL DEFAULT '{}'::jsonb;
ALTER TABLE public.nodes ADD COLUMN IF NOT EXISTS taints jsonb NOT NULL DEFAULT '[]'::jsonb;
