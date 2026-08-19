-- MAIN-332: who a drafted reply may go to, and whether it went.
--
-- Addresses, a subject and a timestamp — no message content, so HC-4's rule
-- that the words live sealed in the object store is untouched. The staffer's
-- address is already in Postgres (the card's body opens "Reported by email from
-- …"), and the subject is already there in distilled form as the card's title;
-- what is new here is the CUSTOMER's address, which nothing recorded before
-- because nothing could send to it.
--
-- **`customer_address` is the delivery's `Reply-To:`, and only that.** A
-- forwarded support mail is authored by the staffer, so `From:` is the staffer
-- and the customer appears nowhere a machine can read — except in `Reply-To:`,
-- which is exactly RFC 5322's "send answers here" and is written by the
-- forwarding staffer rather than by the customer. Reading the address out of
-- the quoted forward instead would take a recipient from attacker-authored body
-- text, which is the one thing HC-1 forbids. Nullable, because most mail
-- carries no `Reply-To:` — and a chain without one simply cannot be replied to
-- by the customer-facing modes, which refuse by name rather than guessing.
ALTER TABLE email_links ADD COLUMN IF NOT EXISTS staffer_address text;
ALTER TABLE email_links ADD COLUMN IF NOT EXISTS customer_address text;
ALTER TABLE email_links ADD COLUMN IF NOT EXISTS subject text;

-- The receipt: when the reply left, and to whom it actually went. Both written
-- together and only after the transport reported a delivery, so a row saying
-- "sent" is never a message that was held back by a send guard — and a row
-- already saying it is what stops a second approve emailing a customer twice.
ALTER TABLE email_links ADD COLUMN IF NOT EXISTS reply_sent_at timestamptz;
ALTER TABLE email_links ADD COLUMN IF NOT EXISTS reply_recipient text;
