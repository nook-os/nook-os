// The Loop panel (MAIN-128): a ticket's own live view of its loop job.
//
// One panel, mounted in the task detail modal. It fetches the ticket's jobs,
// picks the latest, and streams that job's transcript — agent narration folded
// away by default, the system/human turns and the outcome kept prominent. When
// the agent stops to ask a person something, the reply surface (reused whole
// from MAIN-159) is raised right here. A failed run offers a re-run; a finished
// one stays readable. Everything refetches on the live `job_changed` event, so
// the panel fills in without a reload.
import React, { useState } from "react";
import { Link } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Play, RotateCcw, Ban, ChevronRight, ChevronDown, Maximize2 } from "lucide-react";
import { api, type LoopJobTranscriptEntry } from "@nookos/api";
import {
  createLoopJob,
  fetchTaskJobs,
  isActiveJob,
  jobKey,
  jobStateMeta,
  looksLikeMarkdown,
  loopAction,
  stripAnsi,
  taskJobsKey,
} from "./loop";
import { ChatView, type ChatViewMessage } from "@nookos/ui";
import { answerInteraction, useTaskInteractions } from "./Interactions";
import { fileSlug, TranscriptActions } from "./transcriptExport";
import { useLive } from "./live";

/** The job transcript as chat messages (MAIN-237). One shared component now
 *  renders this and team chat, so the loop cannot drift off-theme again — the
 *  bespoke `loop-line` rows this replaces were the fork the MAIN-128 comment
 *  admitted to ("rather than importing the coupled ChatView"). */
export function transcriptMessages(
  lines: LoopJobTranscriptEntry[],
): ChatViewMessage[] {
  return lines.map((l) => ({
    id: l.id,
    // The SOURCE is the author here — `system`, `agent`, `human`. Grouping then
    // falls out for free: a run of agent narration collapses under one header
    // exactly as a run from one person does in team chat.
    authorId: l.source,
    authorName: l.source,
    // ANSI is stripped for the VIEW only; the stored line keeps every escape
    // (MAIN-161 NG-2). The Loop page's own mapping already did both of these —
    // this one had drifted behind it, which is how an agent's `**verdict**`
    // rendered as literal asterisks in the Reviews panel.
    body: stripAnsi(l.content),
    createdAt: l.at,
    markdown: looksLikeMarkdown(l.content),
  }));
}

/** What the activity indicator should say for a job in `state`, or null for no
 *  indicator (MAIN-237 AC-4, MAIN-240 AC-2).
 *
 *  Only a job that could still be producing output gets one. A job paused on a
 *  human is NOT working — it is waiting on you, which the interaction surface
 *  says far better — and a finished job is finished. Showing "working…" in
 *  either case is the specific lie this is meant to prevent: it is the operator's
 *  only cue that the agent is alive.
 *
 *  `turn` is the real signal the streaming adapter reports (`job_turn`), and it
 *  OUTRANKS the inference — that is the whole point of MAIN-240. State can only
 *  ever say "this job is running", which stays true in the gap between turns
 *  when the agent is sitting idle waiting to be steered; the adapter can say
 *  which of those two it actually is.
 *
 *  It is deliberately allowed to silence the indicator and never to raise one:
 *
 *  - `undefined` — no adapter reported. The tmux fallback path (NG-1) never
 *    will, so it keeps the inferred label exactly as before.
 *  - `active: false` — a real "not working right now". Believed, so a job
 *    between turns stops claiming to work.
 *  - `active: true` — agrees with the inference; nothing to add.
 *
 *  `queued` and `claimed` ignore `turn` entirely: their labels describe the
 *  executor, not the agent, and no turn can be in flight before a process
 *  exists. Only `running` is turn-gated. */
export function agentActivityLabel(
  state: string,
  turn?: { active: boolean },
): string | null {
  switch (state) {
    case "queued":
      return "waiting for an executor…";
    case "claimed":
      return "the operator agent is starting…";
    case "running":
      if (turn && !turn.active) return null;
      return "the operator agent is working…";
    default:
      return null;
  }
}

/** The entry action's button — start a spec/decompose run, or re-run. Shared
 *  shape so the panel header and any other home read identically. Disabled with
 *  the reason as its tooltip when a job is already active (AC-1). */
export function LoopActionButton({
  taskType,
  taskId,
  className = "btn small primary",
  onStarted,
}: {
  taskType: string | null | undefined;
  taskId: string;
  className?: string;
  onStarted?: () => void;
}) {
  const qc = useQueryClient();
  const { data: jobs } = useQuery({
    queryKey: taskJobsKey(taskId),
    queryFn: () => fetchTaskJobs(taskId),
  });
  const { kind, label, disabled, reason } = loopAction(taskType, jobs);

  const create = useMutation({
    mutationFn: () => createLoopJob(kind, taskId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: taskJobsKey(taskId) });
      onStarted?.();
    },
  });

  const blocked = disabled || create.isPending;
  return (
    <button
      className={className}
      disabled={blocked}
      title={reason ?? label}
      aria-label={label}
      onClick={() => !blocked && create.mutate()}
    >
      <Play size={11} /> {label}
    </button>
  );
}

/** The panel body once the ticket has at least one job. */
function LoopJobView({
  taskId,
  taskType,
  jobId,
  taskKey,
}: {
  taskId: string;
  taskType: string | null | undefined;
  jobId: string;
  taskKey?: string | null;
}) {
  const qc = useQueryClient();
  const [showAgent, setShowAgent] = useState(false);
  const asks = useTaskInteractions(taskId);
  // The real turn signal for THIS job (MAIN-240 AC-2). Selected by id rather
  // than pulling the whole map, so a turn starting on some other ticket's job
  // does not re-render this panel.
  const turn = useLive((s) => s.jobTurn[jobId]);

  const { data: detail } = useQuery({
    queryKey: jobKey(jobId),
    queryFn: async () =>
      (await api.GET("/api/v1/jobs/{id}", { params: { path: { id: jobId } } })).data,
  });

  const rerun = useMutation({
    mutationFn: () => api.POST("/api/v1/jobs/{id}/rerun", { params: { path: { id: jobId } } }),
    onSuccess: () => qc.invalidateQueries({ queryKey: taskJobsKey(taskId) }),
  });
  const cancel = useMutation({
    mutationFn: () => api.POST("/api/v1/jobs/{id}/cancel", { params: { path: { id: jobId } } }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: taskJobsKey(taskId) });
      qc.invalidateQueries({ queryKey: jobKey(jobId) });
    },
  });

  if (!detail) {
    return <div className="faint small">Loading loop…</div>;
  }

  const meta = jobStateMeta(detail.state);
  const active = isActiveJob(detail);
  const failed = detail.state === "failed";

  const transcript = detail.transcript ?? [];
  const agentLines = transcript.filter((l) => l.source === "agent");
  // Agent narration still folds away by default — the panel is 420px and the
  // raw stream drowns the turns that matter. The fold is the CALLER's job:
  // ChatView renders whatever list it is handed, so this stays a filter.
  const shown = showAgent ? transcript : transcript.filter((l) => l.source !== "agent");

  // The composer answers the pending ask, when there is one. With nothing to
  // answer there is nobody listening, so it says so rather than accepting text
  // that would go nowhere.
  const ask = asks[0];

  return (
    <div className="loop-job">
      <div className="loop-job-bar">
        <span className={`pill ${meta.tone === "muted" ? "" : meta.tone}`}>{meta.label}</span>
        <span className="faint small">{detail.kind}</span>
        {detail.queued_reason && detail.state === "queued" && (
          <span className="small loop-queued-reason" title={detail.queued_reason}>
            {detail.queued_reason}
          </span>
        )}
        <span className="loop-job-bar-actions">
          <TranscriptActions
            lines={transcript}
            filename={`${fileSlug(taskKey ?? "task")}-${jobId.slice(0, 8)}.md`}
          />
          {active && (
            <button
              className="btn small"
              disabled={cancel.isPending}
              onClick={() => cancel.mutate()}
              title="cancel this loop job"
            >
              <Ban size={11} /> Cancel
            </button>
          )}
          {failed && (
            <button
              className="btn small primary"
              disabled={rerun.isPending}
              onClick={() => rerun.mutate()}
              title="run this job again"
            >
              <RotateCcw size={11} /> Re-run
            </button>
          )}
        </span>
      </div>

      {agentLines.length > 0 && (
        <button
          className="loop-fold-toggle"
          onClick={() => setShowAgent((v) => !v)}
          aria-expanded={showAgent}
        >
          {showAgent ? <ChevronDown size={11} /> : <ChevronRight size={11} />}
          {showAgent ? "Hide" : "Show"} agent narration ({agentLines.length} line
          {agentLines.length === 1 ? "" : "s"})
        </button>
      )}

      <ChatView
        messages={transcriptMessages(shown)}
        emptyLabel="No transcript yet — it fills in as the agent works."
        typing={agentActivityLabel(detail.state, turn)}
        disabled={!ask}
        placeholder={ask ? "Answer the agent…" : "Nothing to answer right now"}
        onSend={(body) => {
          if (ask) void answerInteraction(qc, ask, body);
        }}
        beforeComposer={
          ask ? (
            <div className="loop-ask">
              <div className="loop-ask-prompt">{ask.prompt}</div>
              {(ask.choices ?? []).length > 0 && (
                <div className="loop-ask-choices">
                  {(ask.choices ?? []).map((c) => (
                    <button
                      key={c}
                      className="btn small"
                      onClick={() => void answerInteraction(qc, ask, c)}
                    >
                      {c}
                    </button>
                  ))}
                </div>
              )}
            </div>
          ) : null
        }
      />

      <div className="loop-job-foot">
        {/* Start-a-fresh action lives in the header; once a job is here, the way
            to start another is to let this one finish (disabled with a reason
            until then — same rule the button enforces). */}
        <LoopActionButton taskType={taskType} taskId={taskId} className="btn small" />
      </div>
    </div>
  );
}

/**
 * The Loop panel, mounted in `TaskDetail`. No jobs yet → a one-line prompt with
 * the start action; otherwise the latest job's live view. Anyone who can edit
 * the ticket sees it — there is no separate loop permission, so it follows the
 * same (server-enforced) gate as the ticket's other edit actions and renders
 * unconditionally (AC-5).
 */
export function LoopPanel({
  taskId,
  taskType,
  taskKey,
}: {
  taskId: string;
  taskType: string | null | undefined;
  /** The card's human key, for the export filename (MAIN-471 AC-2). */
  taskKey?: string | null;
}) {
  const { data: jobs, isLoading } = useQuery({
    queryKey: taskJobsKey(taskId),
    queryFn: () => fetchTaskJobs(taskId),
  });

  const { label } = loopAction(taskType, jobs);
  const latest = jobs && jobs.length > 0 ? jobs[0] : null;

  return (
    <div className="task-section loop-panel">
      <div className="task-section-h loop-panel-h">
        <span>loop</span>
        {/* The panel stays the compact status light it has always been; the
            room to actually drive a run — seed box, composer, readable drafts —
            is one click away (MAIN-233 AC-4). */}
        <Link
          className="btn small"
          to={`/loop/${taskId}`}
          title="open the full Loop workspace"
        >
          <Maximize2 size={11} /> Open in Loop
        </Link>
        {!latest && !isLoading && <LoopActionButton taskType={taskType} taskId={taskId} />}
      </div>
      {isLoading ? (
        <div className="faint small">Loading loop…</div>
      ) : latest ? (
        <LoopJobView taskId={taskId} taskType={taskType} jobId={latest.id} taskKey={taskKey} />
      ) : (
        <div className="faint small loop-empty">
          {taskType === "epic"
            ? "Run the decomposer to break this epic into sub-tickets."
            : `No loop run yet — “${label}” drafts one for this ticket.`}
        </div>
      )}
    </div>
  );
}
