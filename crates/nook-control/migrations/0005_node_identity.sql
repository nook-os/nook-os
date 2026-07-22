-- A node's durable identity is its KEYPAIR, not its certificate.
--
-- The certificate is the expiring artifact; the key is what proves, months
-- later, that the machine asking for a new one is the machine that enrolled.
-- Recording the public key is what makes self-service renewal possible without
-- a fresh join token — and a machine WILL be offline for arbitrary stretches
-- (a laptop shut for three weeks, a build box unplugged over a holiday), so
-- expiry must never cost a manual re-join.
ALTER TABLE nodes ADD COLUMN IF NOT EXISTS public_key_pem TEXT;

-- The current leaf, kept for observability: an admin watching a rotation needs
-- to see which certificate a node actually holds, not infer it.
ALTER TABLE nodes ADD COLUMN IF NOT EXISTS cert_pem TEXT;
