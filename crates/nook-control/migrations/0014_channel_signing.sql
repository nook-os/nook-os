-- A per-channel signing secret.
--
-- An outbound webhook is an unauthenticated POST arriving at somebody's
-- server. Without a signature the receiver cannot tell a NookOS notification
-- from anyone on the internet who guessed the URL — and webhook URLs leak, into
-- logs, screenshots and repos. Signing makes the receiver's check possible;
-- whether they perform it is theirs to decide, but they must be able to.
--
-- Generated for every channel, not just webhooks: Slack and Discord URLs are
-- themselves secrets, but a self-hosted ntfy or a generic webhook is not.
ALTER TABLE notification_channels
    ADD COLUMN IF NOT EXISTS secret TEXT;

-- Existing rows get one so nothing is left unsigned. gen_random_uuid twice is
-- 256 bits of the same CSPRNG the rest of the schema already relies on.
UPDATE notification_channels
SET secret = replace(gen_random_uuid()::text, '-', '')
             || replace(gen_random_uuid()::text, '-', '')
WHERE secret IS NULL;
