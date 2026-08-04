-- MAIN-388: make a tenant-scoped setting's uniqueness key NULL-free, so that
-- writing one UPDATES instead of silently inserting a duplicate.
--
-- SQLITE-ONLY, and deliberately so: this has no Postgres twin because Postgres
-- is already correct. `migrations/0001_init.sql` declares
--
--     UNIQUE NULLS NOT DISTINCT (tenant_id, scope, user_id, key)
--
-- and there is no such modifier in SQLite — it follows the SQL default where
-- every NULL is distinct. A tenant-scoped setting has `user_id = NULL`, so the
-- `ON CONFLICT (tenant_id, scope, user_id, key)` in `DbSettingRepository::put`
-- never matched anything here: every write INSERTed, and `tenant_value` (an
-- unordered `SELECT … LIMIT`-less read) then returned the FIRST row written.
-- Measured before this file: three writes of one key left three rows, and the
-- reads returned the first value, forever.
--
-- The visible damage was the loop switch. `nook operator loops on` reports
-- success unconditionally — `services::loops::set` ends in `Ok(on)`, returning
-- its own argument rather than a read-back — while `loops::enabled` kept
-- reading whichever value was written first. The switch was write-once in both
-- directions: a tenant turned on could never be turned off.
--
-- The table constraint below cannot be redeclared (SQLite cannot alter one, and
-- rebuilding the table to drop it is a far bigger hammer than this needs), so
-- the fix is an expression index, which is the only form that can hold a
-- `COALESCE`. Leaving the original `UNIQUE (tenant_id, scope, user_id, key)` in
-- place is harmless and was verified rather than assumed: a user-scoped write
-- violates BOTH constraints at once, and the upsert still resolves, because
-- `DO UPDATE` rewrites the very row that violated them.

-- Collapse the duplicates this bug already created, newest wins. `rowid` is
-- SQLite's insertion order, so MAX is the most recent write — the value the
-- operator last asked for and never got. This MUST precede the index: on any
-- database that has drifted, creating it first would simply fail.
DELETE FROM settings
 WHERE rowid NOT IN (
       SELECT MAX(rowid) FROM settings
        GROUP BY tenant_id, scope, COALESCE(user_id, ''), key
 );

-- `COALESCE(user_id, '')` is the NULL-free key. The empty string is safe as the
-- stand-in because `user_id` holds an id and no id is ever ''.
CREATE UNIQUE INDEX IF NOT EXISTS settings_tenant_scope_user_key_uniq
    ON settings (tenant_id, scope, COALESCE(user_id, ''), key);
