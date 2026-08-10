-- SQLite twin of 0057 (hand-authored, per CLAUDE.md). Nullable ADD COLUMN
-- needs no rebuild; `IF NOT EXISTS` is not valid on ADD COLUMN here.
--
-- 0057 on BOTH tracks, which is the number that restores lockstep: the pair
-- takes the highest either set uses plus one, so the offset 0055 opened ends
-- here rather than being carried forward.
ALTER TABLE nodes ADD COLUMN max_loop_jobs integer;
