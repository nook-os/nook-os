-- SQLite twin of 0053 (hand-authored, per CLAUDE.md). Nullable text ADD COLUMN
-- needs no rebuild; `IF NOT EXISTS` is not valid on ADD COLUMN here.
ALTER TABLE loop_jobs ADD COLUMN build_outcome text;
ALTER TABLE loop_jobs ADD COLUMN build_fingerprint text;
