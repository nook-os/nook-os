-- Local-account email verification (MAIN-30): the token half of the round-trip.
--
-- A signed-in local user requests verification; we issue a single-use, expiring
-- token, store only its SHA-256 here, and email the plaintext in a link. The
-- confirm endpoint hashes the presented token, matches an unconsumed/unexpired
-- row, marks it consumed, and records the verification via the verified-email
-- model (a `local`-issuer identity carrying `email_verified_at`).
--
-- Only the hash is ever at rest, so a database dump does not hand out working
-- verification links. Append-only and idempotent so a database that already has
-- the table converges.
CREATE TABLE IF NOT EXISTS public.email_verification_tokens (
    id          uuid PRIMARY KEY,
    user_id     uuid NOT NULL REFERENCES public.users(id) ON DELETE CASCADE,
    email       text NOT NULL,
    token_hash  text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz NOT NULL,
    consumed_at timestamptz
);

-- At most one LIVE (unconsumed) token per user: re-requesting replaces rather
-- than stacks (AC-3). Consumed rows are kept for the idempotency check, so the
-- index is partial on unconsumed.
CREATE UNIQUE INDEX IF NOT EXISTS email_verification_one_live_per_user
    ON public.email_verification_tokens (user_id) WHERE consumed_at IS NULL;

-- Confirm looks up by hash.
CREATE INDEX IF NOT EXISTS email_verification_token_hash_idx
    ON public.email_verification_tokens (token_hash);
