-- Channel archive (MAIN-49 AC-1). Archived channels are hidden from the default
-- list and refuse new posts, but their history stays readable. Nullable
-- timestamp rather than a boolean so "when" is recorded for free; idempotent so
-- a database that already has the column converges instead of failing.
ALTER TABLE chat_channels ADD COLUMN IF NOT EXISTS archived_at timestamptz;
