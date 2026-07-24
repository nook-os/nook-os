-- MAIN-52: a persistent record of real outbound mail sends.
--
-- The quota guard (MAIL_MAX_PER_MONTH / MAIL_MAX_PER_DAY) must survive restarts
-- and deploys, so the count is derived from this table rather than an in-memory
-- counter. One row per REAL send (captured / gated / quota-blocked sends are
-- logged, not recorded here). Deployment-level, not tenant-scoped: the Postmark
-- allowance is a property of the deployment, and the recipient address is
-- reduced to its domain so this is an audit of volume, not a log of who was
-- mailed.
CREATE TABLE IF NOT EXISTS mail_sends (
    id uuid PRIMARY KEY,
    sent_at timestamptz NOT NULL DEFAULT now(),
    -- 'transactional' | 'notification'
    category text NOT NULL,
    recipient_domain text NOT NULL
);

-- The quota count is "rows since the start of the month/day", so the window
-- scan is on sent_at.
CREATE INDEX IF NOT EXISTS mail_sends_sent_at_idx ON mail_sends (sent_at);
