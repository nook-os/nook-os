// What the builder is DOING, and what it PRODUCED (MAIN-387 AC-7/AC-8).
//
// A build run's output is a branch and a pull request. Everything on screen
// before this said so only inside the transcript, so "what did that pass
// produce" meant scrolling a log for the `gh pr create` line — on a finished
// run as much as a live one.
//
// Two components, one join. [`BuilderStrip`] is the live half — which card,
// which machine, working or between turns — and [`BuildOutcome`] is the
// durable half, which is the same three facts a finished run leaves behind.
// They share `useBuildRunFacts` so a card key shown in one cannot disagree with
// the other.
//
// Read-only, deliberately (NG-4). The point of both is that the transcript
// stays one click behind them, not that they become a place to drive a run.
import React from "react";
import { Link } from "react-router-dom";
import { ExternalLink, GitBranch } from "lucide-react";
import { Pill } from "@nookos/ui";
import { buildOutcomeWords, useBuildRunFacts, type BuildRunRow } from "./buildLoop";
import { agentActivityLabel, jobStateMeta } from "./loop";
import { useLive } from "./live";

/** The three durable facts, rendered the same way wherever they appear. Each is
 *  omitted rather than shown empty: a run that has not opened a PR yet has no
 *  PR, and a dash in its place reads as a broken link. */
function Facts({
  taskId,
  taskKey,
  branch,
  prUrl,
}: {
  taskId: string | null;
  taskKey: string | null;
  branch: string | null;
  prUrl: string | null;
}) {
  return (
    <>
      {taskKey && (
        <Link
          className="mono bright"
          to={taskId ? `/loop/${taskId}` : `/board?task=${taskKey}`}
          data-testid="build-ticket"
          // The transcript, one click behind the strip (AC-8) — the Loop
          // workspace opens on this card's newest run. Before the job join
          // lands there is no id to route by, so the card itself is the link.
          title={taskId ? `open ${taskKey}'s run` : `open ${taskKey}`}
        >
          {taskKey}
        </Link>
      )}
      {branch && (
        <span className="mono faint small" data-testid="build-branch" title="the run's branch">
          <GitBranch size={11} /> {branch}
        </span>
      )}
      {prUrl && (
        <a
          className="small"
          href={prUrl}
          data-testid="build-pr"
          target="_blank"
          rel="noreferrer"
          title={prUrl}
        >
          <ExternalLink size={11} /> {prLabel(prUrl)}
        </a>
      )}
    </>
  );
}

/** `PR #443` from the URL a run reported. The number is what a person says out
 *  loud; the whole URL is on the `title` for anyone who wants the repo too. */
export function prLabel(url: string): string {
  const n = /\/pull\/(\d+)/.exec(url)?.[1];
  return n ? `PR #${n}` : "pull request";
}

/**
 * What a build run produced — its ticket, branch and PR as first-class
 * elements, present WHILE it runs and after it ends (AC-7).
 *
 * Renders nothing for anything that is not a build: a spec run's output is the
 * ticket it filled in, and claiming a branch for it would be an invention.
 */
export function BuildOutcome({
  job,
  workspaceId,
  showBranch = true,
}: {
  job: { id: string; kind: string; workspace_id?: string | null } | null | undefined;
  /** The repo, when the caller already knows it — the branch is resolved
   *  through its checkouts. Falls back to the job's own. */
  workspaceId?: string | null;
  /** Off where the caller is ALREADY showing the branch (MAIN-559 AC-7): the
   *  runs panel's header carries it, and one fact rendered twice a line apart
   *  reads as two. */
  showBranch?: boolean;
}) {
  const isBuild = job?.kind === "build";
  const facts = useBuildRunFacts(
    isBuild ? job?.id : null,
    workspaceId ?? job?.workspace_id ?? null,
  );
  if (!isBuild) return null;
  const outcome = buildOutcomeWords(facts.outcome);
  const branch = showBranch ? facts.branch : null;
  if (!facts.taskKey && !branch && !facts.prUrl && !outcome) return null;
  return (
    <span className="build-facts" data-testid="build-outcome">
      <Facts
        taskId={facts.taskId}
        taskKey={facts.taskKey}
        branch={branch}
        prUrl={facts.prUrl}
      />
      {outcome && (
        <Pill tone={facts.outcome === "pr_opened" ? "ok" : "warn"}>{outcome}</Pill>
      )}
    </span>
  );
}

/**
 * The builder, right now (AC-8): which card, which machine, and whether the
 * agent is mid-turn — plus the branch and PR the moment they exist.
 *
 * `run` is the workspace's newest LIVE build run, chosen by the caller so the
 * panel and Mission Control cannot pick differently. Renders nothing without
 * one: an idle repo's reason is `buildLoopWhy`'s to state, and a strip saying
 * "nothing" would compete with it.
 */
export function BuilderStrip({
  run,
  workspaceId,
}: {
  run: BuildRunRow | null;
  workspaceId: string;
}) {
  // Selected by id rather than pulling the whole map, so a turn starting on
  // another repo's run does not repaint this one.
  const turn = useLive((s) => (run ? s.jobTurn[run.id] : undefined));
  const facts = useBuildRunFacts(run?.id ?? null, workspaceId);
  if (!run) return null;
  const meta = jobStateMeta(run.state);
  const activity = agentActivityLabel(run.state, turn);
  // The listing's key is available immediately; the job join fills in a moment
  // later. Preferring the listing's stops the strip flickering through a
  // keyless state on every repaint.
  const taskKey = run.task_key ?? facts.taskKey;
  return (
    // A span, not a div: one of this component's two homes is a Mission Control
    // repo header, which is inline — a block child there is invalid nesting and
    // React says so.
    <span className="builder-strip" data-testid="builder-strip">
      <Pill tone={meta.tone === "muted" ? "dim" : meta.tone}>{meta.label}</Pill>
      <Facts
        taskId={facts.taskId}
        taskKey={taskKey}
        branch={facts.branch}
        prUrl={facts.prUrl}
      />
      {facts.nodeName && (
        <span className="faint small" data-testid="builder-node">
          on {facts.nodeName}
        </span>
      )}
      {/* The real turn signal, not an inference from state (MAIN-240): a run
          that is `running` but between turns is waiting to be steered, and
          saying "working" there is the one lie this indicator must not tell. */}
      {activity && (
        <span className="faint small" data-testid="builder-activity">
          {activity}
        </span>
      )}
    </span>
  );
}
