-- Verified-email model (MAIN-29): a platform-level "this email is verified"
-- fact, so email can finally be a trusted attribute the way person_id made
-- membership trustworthy (MAIN-12).
--
-- It lives on `identities`, not `users`: verification is a property of HOW you
-- proved the address (an IdP asserting email_verified, or — later — a local
-- verification round-trip), and one person can hold several identities. A local
-- account has no identities row at all, so it is unverified by construction,
-- which is exactly right.
--
-- Nullable with no default: existing rows, and any identity created without a
-- verified claim, are correctly null. It is only ever set from a real
-- verification, never from the mere presence of an email string.
--
-- Append-only and idempotent (IF NOT EXISTS) so a database that already has the
-- column converges instead of failing.
ALTER TABLE public.identities
    ADD COLUMN IF NOT EXISTS email_verified_at timestamptz;
