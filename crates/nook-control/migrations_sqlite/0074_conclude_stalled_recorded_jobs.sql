-- SQLite twin of 0074 (hand-authored, per CLAUDE.md). `now()` → the one
-- timestamp form this engine writes (`nook_db::sqlite_time::NOW_SQL`).
UPDATE loop_jobs
   SET state = 'completed', updated_at = strftime('%Y-%m-%d %H:%M:%f','now')
 WHERE state IN ('claimed', 'running')
   AND (build_outcome IS NOT NULL OR review_verdict IS NOT NULL);
