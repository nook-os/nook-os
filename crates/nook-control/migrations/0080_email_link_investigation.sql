-- MAIN-331: what the read-only investigate run produced, on the chain it was
-- seeded from.
--
-- Two columns because they have two different confidentialities, and flattening
-- them into one would force the stricter rule onto both:
--
--   * `findings` is the run's OWN analysis — what it reproduced, where the code
--     goes wrong. It is read on an inbox and on the card, so it is stored as
--     text like every other thing this product shows a human.
--   * `draft_reply_enc` is a reply addressed to the reporter, and a reply
--     quotes what they wrote. That is customer content, so it lives here the
--     way the raw message does: `Vault::encrypt` ciphertext, never plaintext in
--     a database dump (HC-4).
--
-- Both are nullable: a link exists from the moment a delivery is accepted, and
-- the investigation lands minutes later — or never, for a tenant with no owner
-- to request the run as.
ALTER TABLE email_links ADD COLUMN IF NOT EXISTS findings text;
ALTER TABLE email_links ADD COLUMN IF NOT EXISTS draft_reply_enc bytea;
