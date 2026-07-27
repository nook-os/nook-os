-- MAIN-113: direct messages. A DM is a `dm`-owner_type channel whose members
-- are recorded per PERSON (the cross-tenant identity org channels established),
-- so the same person reaches a DM from any of their tenants.
--
-- Runs with search_path=chat (unqualified names resolve there), like the rest of
-- the chat schema. Idempotent and additive.

-- Widen the owner_type CHECK to admit 'dm'. The original constraint is an inline
-- unnamed CHECK, which Postgres names `chat_channels_owner_type_check`; drop it
-- (if present) and re-add the widened, explicitly-named version.
ALTER TABLE chat_channels DROP CONSTRAINT IF EXISTS chat_channels_owner_type_check;
ALTER TABLE chat_channels
    ADD CONSTRAINT chat_channels_owner_type_check
    CHECK (owner_type IN ('org', 'tenant', 'dm'));

-- Who is in a DM. Person-keyed (not user-keyed like chat_channel_members) so a
-- DM is reachable by the person across tenants. No cross-schema FK to
-- public.persons by design (the two services stay loosely coupled); membership
-- integrity is enforced in application code.
CREATE TABLE IF NOT EXISTS chat_channel_participants (
    channel_id uuid NOT NULL REFERENCES chat_channels (id) ON DELETE CASCADE,
    person_id uuid NOT NULL,
    PRIMARY KEY (channel_id, person_id)
);

-- "Which DMs is this person in?" — the DM list and participant checks.
CREATE INDEX IF NOT EXISTS chat_channel_participants_person_idx
    ON chat_channel_participants (person_id);
