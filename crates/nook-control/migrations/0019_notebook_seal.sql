-- MAIN-100: optional per-note zero-knowledge sealing for the personal notebook.
--
-- Two additions, both append-only and idempotent (re-running converges):
--
-- 1. `person_vaults` — a PERSON-level app-password vault, mirroring `user_vaults`
--    (0001_init) but keyed by `person_id` (the cross-org identity the notebook
--    is owned by, MAIN-66) instead of a per-tenant `user_id`. The server stores
--    only a KDF salt and a one-way verifier, never the password or the key it
--    derives, so it can reject a wrong password without ever being able to
--    decrypt on its own. Set-once, like `user_vaults`.
--
-- 2. Nullable seal columns on `user_notes`, the `workspace_secrets` shape
--    (kdf_salt + verifier). A note is "sealed" iff `sealed_salt IS NOT NULL`;
--    sealed and unsealed rows coexist. For a sealed note `content_enc` holds the
--    CLIENT-produced sealed ciphertext, additionally vault-wrapped at rest
--    (as `gitops.rs::store_sealed` does) — the server never receives the note
--    plaintext and cannot open the seal without the person's app password.

CREATE TABLE IF NOT EXISTS public.person_vaults (
    person_id uuid PRIMARY KEY,
    kdf_salt bytea NOT NULL,
    verifier bytea NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE public.user_notes
    ADD COLUMN IF NOT EXISTS sealed_salt bytea,
    ADD COLUMN IF NOT EXISTS sealed_verifier bytea;
