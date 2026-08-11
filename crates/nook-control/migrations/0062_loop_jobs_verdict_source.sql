-- MAIN-516: WHO concluded a review verdict, when the answer is "no agent did".
--
-- A merge that makes an already-approved PR conflict used to strand it: the
-- hygiene pass labelled it `loop-changes-requested`, but the repair queue reads
-- the JOB LEDGER (`rejected_review_heads`) and no row ever said the PR was
-- rejected. The PR's recorded verdict stayed `approved` — correctly, for the
-- head that was reviewed — so no repair was raised, and a conflict moves no
-- head, so nothing re-triggered. A stable deadlock.
--
-- The hygiene pass now records the `changes_requested` the queue reads. This
-- column is what keeps that honest: NULL is an agent's own conclusion, and
-- `conflict` is the control plane's, from a merge conflict, with no findings
-- behind it. A verdict no agent produced must never read as one.
ALTER TABLE loop_jobs ADD COLUMN IF NOT EXISTS review_verdict_source text;

-- One conflict verdict per head, enforced where two control-plane replicas
-- converging the same instant cannot argue with it — the same job 0046's index
-- does for live runs. Without it, "does this head already have one?" is a read
-- followed by a write, and both replicas pass the read.
CREATE UNIQUE INDEX IF NOT EXISTS loop_jobs_one_conflict_verdict_per_head
    ON loop_jobs (workspace_id, review_pr_number, review_head_sha)
    WHERE review_verdict_source = 'conflict';
