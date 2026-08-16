-- The IMAP poller and what it has already ingested (MAIN-333).
--
-- 0079 is free on BOTH tracks, which is the rule the numbering follows: 0078 is
-- the highest either set uses, so the twins land together and lockstep holds.
--
-- It was 0078 while this branch was open, and MAIN-330 took that number on both
-- tracks first. Git does not conflict on it — the two files have different
-- names, so both simply arrive — and the failure surfaces much later as
-- `UNIQUE constraint failed: _sqlx_migrations.version`, which is the same trap
-- MAIN-502 records. Renumbered on the rebase, both halves together.

-- One poller per tenant, because `email.inbound` already says one support
-- address per tenant and a second mailbox feeding the same address would only
-- be two ways to file the same message. The tenant IS the key.
--
-- A table rather than another `settings` row (which is where `email.inbound`
-- lives) for one reason: AC-4. A settings value is plaintext JSON written
-- through a generic endpoint, and there is no seam there to seal a field on its
-- way in — so a password stored that way is a password at rest in the clear.
-- Here the credential has a column of its own, holding only what
-- `crypto::Vault` produced.
CREATE TABLE IF NOT EXISTS email_pollers (
    tenant_id uuid PRIMARY KEY REFERENCES tenants(id) ON DELETE CASCADE,
    host text NOT NULL,
    port integer NOT NULL,
    username text NOT NULL,
    -- AES-256-GCM, `nonce || ciphertext`, exactly as `git_credentials.secret_enc`
    -- and `workspaces.webhook_secret_enc` hold theirs. NOT NULL: a poller with
    -- no credential cannot log in, so there is no half-configured row to
    -- represent.
    password_enc bytea NOT NULL,
    mailbox text NOT NULL,
    poll_interval_secs integer NOT NULL,
    enabled boolean NOT NULL DEFAULT true,

    -- IMAP's own high-water mark, so an ordinary poll asks the server for the
    -- messages that arrived since the last one instead of the whole mailbox.
    -- `uid_validity` is what makes that safe: the server may renumber a mailbox
    -- at any time, and it says so by changing this value — at which point
    -- `last_uid` means nothing and the poller starts over. The message-id
    -- ledger below is what stops "starts over" meaning "files everything twice".
    uid_validity bigint,
    last_uid bigint NOT NULL DEFAULT 0,

    -- When the last poll ran, and what went wrong if it did. `last_polled_at`
    -- is also the claim: the sweep updates it conditionally, so one replica
    -- polls a mailbox and the others see zero rows (the shape every reaper here
    -- already uses).
    last_polled_at timestamptz,
    last_error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- AC-3. One row per message this deployment has decided about, keyed by the
-- message's own identity rather than by anything the transport assigned.
--
-- **Written BEFORE the message is processed, not after.** The row is a claim:
-- an insert that conflicts means somebody else already has this message, and
-- the poller skips it. Recording it afterwards would leave the window between
-- filing and recording open, which is exactly the window two replicas polling
-- one mailbox — or one poller overlapping its own slow run — occupy.
--
-- Scoped by source as well as tenant so the same message arriving by two
-- transports is still one card per transport's own ledger, and so a future
-- source cannot be surprised by a key it never wrote.
CREATE TABLE IF NOT EXISTS inbound_email_seen (
    tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    source text NOT NULL,
    -- The `Message-Id` header when the message carries one, else a digest of
    -- the raw bytes — see `email_imap::dedupe_key` for why the fallback is not
    -- optional.
    message_id text NOT NULL,
    -- The card it became. NULL while the claim is held and for a message the
    -- trust gate dropped, which is a real state and not a missing one: nothing
    -- was filed, and re-polling must still not re-decide it.
    task_id uuid REFERENCES tasks(id) ON DELETE SET NULL,
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, source, message_id)
);
