-- MAIN-442: give the SQLite track ONE timestamp form, so a TEXT timestamp
-- column compares the way its type name claims to.
--
-- SQLITE-ONLY, and deliberately so: this has no Postgres twin, and must never
-- get one. Postgres stores a real `timestamptz` and compares instants; it is
-- already correct, and a format change there would be a data migration across
-- nearly every table. The number 0055 is therefore BURNED on the Postgres side
-- — the next Postgres migration is 0056 — exactly as 0038 burned its own.
--
-- ── the defect ──────────────────────────────────────────────────────────────
--
-- SQLite has no timestamp type. `timestamptz` maps to TEXT (docs/db-dialect-
-- audit.md), and TEXT compares byte by byte, so two renderings of one instant
-- are two different values. Three writers disagreed:
--
--   DEFAULT CURRENT_TIMESTAMP   2026-08-06 13:28:36
--   a bound DateTime<Utc>       2026-08-06T13:28:36.411979+00:00   (sqlx, RFC 3339)
--   datetime('now', …)          2026-08-06 13:28:36
--
-- Nothing errored, because sqlx's DECODER accepts all three. What broke was
-- comparison, silently:
--
--   (a) `repo/tasks.rs`'s optimistic-concurrency guard ends
--       `AND (… IS NULL OR updated_at = $11)`. A value read out of the column
--       and bound straight back never compared equal to it, so EVERY guarded
--       update matched 0 rows and reported Conflict — including one whose
--       version was current.
--   (b) Second resolution cannot order two rows written in the same second:
--       they are byte-identical, so `created_at > cursor` reported a message
--       posted in the cursor's own second as already read.
--
-- ── the form ────────────────────────────────────────────────────────────────
--
-- `strftime('%Y-%m-%d %H:%M:%f','now')` — `2026-08-06 13:28:36.411` — stated
-- once in `nook_db::sqlite_time`, which is also where the BINDER now renders
-- from, so the two halves cannot drift. Milliseconds is SQLite's own clock
-- resolution and `%f`'s exact width; both halves must render the same width or
-- equality breaks again, so that width is this one.
--
-- ── why the schema is rewritten in place ────────────────────────────────────
--
-- SQLite cannot ALTER a column's default, and the alternative — rebuilding all
-- 53 tables through the 12-step procedure, with their foreign keys and indexes
-- — is a far bigger hammer than a default clause needs, and a far riskier one
-- (0038 declined the same hammer for the same reason). A DEFAULT is schema
-- TEXT: it is not part of any stored record, so editing it changes nothing
-- already written and cannot misread anything. `PRAGMA writable_schema` is the
-- documented way to make exactly that kind of change.
--
-- The replace is anchored on the full `DEFAULT CURRENT_TIMESTAMP`, which is the
-- only spelling this track uses — the 80 declarations across `0001` and its
-- deltas are byte-identical. `_sqlx_migrations` is excluded because sqlx
-- declares its `installed_on` that way too, and that table's DDL is sqlx's.
PRAGMA writable_schema = ON;

UPDATE sqlite_master
   SET sql = replace(
                 sql,
                 'DEFAULT CURRENT_TIMESTAMP',
                 'DEFAULT (strftime(''%Y-%m-%d %H:%M:%f'',''now''))'
             )
 WHERE type = 'table'
   AND name NOT LIKE 'sqlite_%'
   AND name <> '_sqlx_migrations'
   AND sql LIKE '%DEFAULT CURRENT_TIMESTAMP%';

PRAGMA writable_schema = RESET;

-- Editing `sqlite_master` by hand does NOT bump `schema_version`, and a
-- connection only re-reads the schema when that number moves. So any OTHER
-- connection already open on this file — a test bed, a second pool, a `sqlx`
-- shell — would go on inserting the OLD default while this one wrote the new:
-- the exact silent divergence this file exists to end. Measured, not assumed:
-- a connection opened before the rewrite kept writing second-resolution values
-- until this pair of statements was added. Ordinary DDL does bump the version,
-- so a scratch table created and dropped is what forces the re-read.
CREATE TABLE _nook_schema_reload (x);
DROP TABLE _nook_schema_reload;

-- ── the values already written ──────────────────────────────────────────────
--
-- The new default only governs rows written from here. Rows already in the
-- database keep whichever of the three forms wrote them, and a stale form is
-- not a cosmetic difference: an `updated_at` that is one second wide can never
-- satisfy the guard above, so a task written before this migration could never
-- be edited again. Every timestamp column is therefore rewritten to the
-- canonical form — not only the defaulted ones, because a column written by a
-- bind (`expires_at`, `claim_expires_at`, …) holds the RFC 3339 shape, whose
-- `T` sorts AFTER every space-separated value on the same date.
--
-- `strftime` returns NULL for anything it cannot parse, and a NOT NULL column
-- would then abort the whole migration, so each value falls back to itself:
-- a value we cannot read is left exactly as found rather than destroyed.
UPDATE boards
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       updated_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', updated_at), updated_at);

UPDATE bus_outbox
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at);

UPDATE chat_channel_categories
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at);

UPDATE chat_channel_members
   SET joined_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', joined_at), joined_at);

UPDATE chat_channels
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       archived_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', archived_at), archived_at);

UPDATE chat_message_revisions
   SET acted_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', acted_at), acted_at);

UPDATE chat_messages
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       edited_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', edited_at), edited_at),
       deleted_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', deleted_at), deleted_at);

UPDATE chat_reactions
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at);

UPDATE chat_read_cursors
   SET last_read_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', last_read_at), last_read_at);

UPDATE email_verification_tokens
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       expires_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', expires_at), expires_at),
       consumed_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', consumed_at), consumed_at);

UPDATE events
   SET occurred_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', occurred_at), occurred_at);

UPDATE feedback
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       updated_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', updated_at), updated_at);

UPDATE git_credentials
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at);

UPDATE identities
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       email_verified_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', email_verified_at), email_verified_at);

UPDATE interactions
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       updated_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', updated_at), updated_at),
       answered_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', answered_at), answered_at);

UPDATE invites
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       expires_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', expires_at), expires_at);

UPDATE join_tokens
   SET expires_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', expires_at), expires_at),
       used_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', used_at), used_at),
       created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at);

UPDATE labels
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at);

UPDATE loop_job_transcript
   SET at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', at), at);

UPDATE loop_jobs
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       updated_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', updated_at), updated_at);

UPDATE mail_sends
   SET sent_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', sent_at), sent_at);

UPDATE managed_content
   SET updated_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', updated_at), updated_at);

UPDATE node_workspaces
   SET discovered_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', discovered_at), discovered_at),
       last_scanned_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', last_scanned_at), last_scanned_at),
       missing_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', missing_at), missing_at);

UPDATE nodes
   SET last_seen_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', last_seen_at), last_seen_at),
       lease_expires_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', lease_expires_at), lease_expires_at),
       created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       updated_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', updated_at), updated_at),
       cert_not_after = COALESCE(strftime('%Y-%m-%d %H:%M:%f', cert_not_after), cert_not_after),
       revoked_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', revoked_at), revoked_at);

UPDATE notes
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       updated_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', updated_at), updated_at);

UPDATE notification_channels
   SET last_ok_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', last_ok_at), last_ok_at),
       created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       updated_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', updated_at), updated_at);

UPDATE notifications
   SET read_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', read_at), read_at),
       created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at);

UPDATE org_visibility_policy
   SET changed_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', changed_at), changed_at);

UPDATE orgs
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       updated_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', updated_at), updated_at);

UPDATE person_vaults
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at);

UPDATE role_bindings
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at);

UPDATE session_port_leases
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at);

UPDATE sessions
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       updated_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', updated_at), updated_at),
       ended_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', ended_at), ended_at);

UPDATE sessions_auth
   SET expires_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', expires_at), expires_at),
       created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at);

UPDATE skills
   SET updated_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', updated_at), updated_at);

UPDATE task_comments
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       updated_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', updated_at), updated_at);

UPDATE task_description_revisions
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at);

UPDATE task_relations
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at);

UPDATE tasks
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       updated_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', updated_at), updated_at),
       archived_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', archived_at), archived_at),
       claim_expires_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', claim_expires_at), claim_expires_at);

UPDATE tenant_cas
   SET not_after = COALESCE(strftime('%Y-%m-%d %H:%M:%f', not_after), not_after),
       created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       retired_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', retired_at), retired_at);

UPDATE tenant_members
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at);

UPDATE tenants
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       updated_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', updated_at), updated_at);

UPDATE themes
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at);

UPDATE user_note_folders
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       updated_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', updated_at), updated_at);

UPDATE user_notes
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       updated_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', updated_at), updated_at);

UPDATE user_passkeys
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       last_used_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', last_used_at), last_used_at);

UPDATE user_tokens
   SET last_used_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', last_used_at), last_used_at),
       expires_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', expires_at), expires_at),
       created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at);

UPDATE user_vaults
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at);

UPDATE users
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       updated_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', updated_at), updated_at);

UPDATE work_queue
   SET not_before = COALESCE(strftime('%Y-%m-%d %H:%M:%f', not_before), not_before),
       enqueued_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', enqueued_at), enqueued_at),
       locked_until = COALESCE(strftime('%Y-%m-%d %H:%M:%f', locked_until), locked_until);

UPDATE work_queue_dead
   SET enqueued_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', enqueued_at), enqueued_at),
       died_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', died_at), died_at);

UPDATE workspace_secrets
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       updated_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', updated_at), updated_at);

UPDATE workspaces
   SET created_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', created_at), created_at),
       updated_at = COALESCE(strftime('%Y-%m-%d %H:%M:%f', updated_at), updated_at);

