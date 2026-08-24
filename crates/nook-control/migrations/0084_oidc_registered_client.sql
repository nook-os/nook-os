-- A client this instance registered for ITSELF at an IdP (RFC 7591), so OIDC
-- can be configured with an issuer alone (MAIN-651).
--
-- Keyed by issuer, not by a single row: pointing an instance at a different IdP
-- must register there rather than present a client id that IdP never issued.
--
-- There is deliberately no secret column. Registration asks for a PUBLIC client
-- (`token_endpoint_auth_method: none`) and declines when the IdP will not issue
-- one, so this table never holds a credential -- nothing here to encrypt, and
-- nothing to leak. The authorization code is bound by PKCE, which the login
-- flow already sends.
CREATE TABLE IF NOT EXISTS public.oidc_registered_client (
    issuer text PRIMARY KEY,
    client_id text NOT NULL,
    registered_at timestamptz NOT NULL DEFAULT now()
);
