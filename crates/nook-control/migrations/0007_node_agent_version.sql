-- What version of the agent each node is running.
--
-- Nothing recorded this, so "which machines are behind?" had no answer short of
-- opening a terminal on each one — and a fleet you cannot survey is a fleet you
-- cannot keep current. The agent reports it on every register, so it is
-- accurate as of the last reconnect rather than the last time someone looked.
ALTER TABLE nodes ADD COLUMN IF NOT EXISTS agent_version TEXT;
