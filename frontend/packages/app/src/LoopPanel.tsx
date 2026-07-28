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
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Play, RotateCcw, Ban, ChevronRight, ChevronDown } from "lucide-react";
import { api, type LoopJobTranscriptEntry } from "@nookos/api";
import {
  createLoopJob,
  fetchTaskJobs,
  isActiveJob,
  jobKey,
  jobStateMeta,
  loopAction,
  taskJobsKey,
} from "./loop";
import { TaskInteractions } from "./Interactions";

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

/** One transcript line. Mirrors the dense chat message row — an author tag and a
 *  body — using the app's own tokens rather than importing the coupled ChatView.
 *  `system`/`human` (and anything that isn't agent narration) render prominent;
 *  agent lines are folded by the caller. */
function TranscriptLine({ line }: { line: LoopJobTranscriptEntry }) {
  return (
    <div className={`loop-line loop-line-${line.source}`}>
      <div className="loop-line-head">
        <span className="loop-line-src">{line.source}</span>
        <span className="faint small loop-line-ts">
          {new Date(line.at).toLocaleTimeString()}
        </span>
      </div>
      <div className="loop-line-body">{line.content}</div>
    </div>
  );
}

/** The panel body once the ticket has at least one job. */
function LoopJobView({
  taskId,
  taskType,
  jobId,
}: {
  taskId: string;
  taskType: string | null | undefined;
  jobId: string;
}) {
  const qc = useQueryClient();
  const [showAgent, setShowAgent] = useState(false);

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
  const waiting = detail.state === "waiting_on_human";
  const failed = detail.state === "failed";

  const transcript = detail.transcript ?? [];
  const agentLines = transcript.filter((l) => l.source === "agent");
  const prominent = transcript.filter((l) => l.source !== "agent");

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

      {/* The agent is blocked on a person — raise the answer surface right here,
          reusing the MAIN-159 component whole (options as chips + free text; the
          answer resumes the job server-side). */}
      {waiting && (
        <div className="loop-waiting">
          <div className="loop-waiting-h">The agent is waiting on a human.</div>
          <TaskInteractions taskId={taskId} />
        </div>
      )}

      {transcript.length === 0 ? (
        <div className="faint small loop-empty">
          No transcript yet — it fills in as the agent works.
        </div>
      ) : (
        <div className="loop-transcript">
          {prominent.map((l) => (
            <TranscriptLine key={l.id} line={l} />
          ))}
          {agentLines.length > 0 && (
            <div className="loop-agent-fold">
              <button
                className="loop-fold-toggle"
                onClick={() => setShowAgent((v) => !v)}
                aria-expanded={showAgent}
              >
                {showAgent ? <ChevronDown size={11} /> : <ChevronRight size={11} />}
                {showAgent ? "Hide" : "Show"} agent narration ({agentLines.length} line
                {agentLines.length === 1 ? "" : "s"})
              </button>
              {showAgent &&
                agentLines.map((l) => <TranscriptLine key={l.id} line={l} />)}
            </div>
          )}
        </div>
      )}

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
}: {
  taskId: string;
  taskType: string | null | undefined;
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
        {!latest && !isLoading && <LoopActionButton taskType={taskType} taskId={taskId} />}
      </div>
      {isLoading ? (
        <div className="faint small">Loading loop…</div>
      ) : latest ? (
        <LoopJobView taskId={taskId} taskType={taskType} jobId={latest.id} />
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
