-- The owner's veto on operator-authorize (MAIN-276 AC-6).
--
-- Authorizing a runtime on a fleet node is UNGATED for the deployment operator
-- by default: it is their hardware, and who pays for the AI is a product
-- decision rather than a security one. Two bounds keep that honest — the owner
-- is notified every time, and the owner may decline the capability on a
-- specific machine. This column is the second.
--
-- `false` by default, so the capability exists everywhere until an owner
-- withdraws it on a machine they own. Making it opt-IN would have meant the
-- operator could authorize nothing on the day this ships, which is the opposite
-- of the card.
--
-- Note what this does NOT gate: whether any workload may RUN on the node.
-- Authorize and permit-work are two separate gates (MAIN-278 owns the second).
ALTER TABLE public.nodes
    ADD COLUMN IF NOT EXISTS operator_authorize_optout boolean NOT NULL DEFAULT false;
