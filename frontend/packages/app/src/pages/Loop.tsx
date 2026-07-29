// The Loop workspace (MAIN-233): a full page for driving one ticket's loop job.
//
// The compact panel in the ticket modal (MAIN-128) is a status light — 420px,
// no composer, "no transcript yet" and nowhere to type. Speccing needs room, so
// this is the same job given the whole screen: the transcript is the main
// column, drafts render as markdown where they land, the agent's questions get
// their answer controls inline, and a persistent bar at the bottom is where you
// actually talk to the run.
//
// The bar is the point. What it is FOR depends on the job (`composerMode`):
// before there is one it is the SEED — the opening idea you want the agent to
// start from (MAIN-231) rather than a bare Play button; while the job runs it
// posts STEERING MESSAGES; once the job is terminal there is no session left to
// talk to, so it says so instead of offering a box that can only fail.
//
// Everything refetches on the live `job_changed` event, so a run streams in
// without a reload.
import React, { useEffect, useRef, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ArrowLeft,
  Ban,
  ExternalLink,
  Play,
  RotateCcw,
  Send,
} from "lucide-react";
import { api, type LoopJob, type LoopJobTranscriptEntry } from "@nookos/api";
import { Empty, Markdown, Panel } from "@nookos/ui";
import {
  composerMode,
  createLoopJob,
  fetchTaskJobs,
  filedKeys,
  jobKey,
  jobStateMeta,
  looksLikeDraft,
  loopAction,
  postJobMessage,
  stripAnsi,
  taskJobsKey,
} from "../loop";
import { InteractionAnswer } from "../Interactions";

/** The ticket this workspace is anchored to. Keys and uuids both resolve
 *  server-side (MAIN-209), so the route accepts whichever the caller had. */
function useTask(taskId: string) {
  return useQuery({
    queryKey: ["task", taskId],
    queryFn: async () =>
      // The detail endpoint wraps the card in `.task` alongside its comments
      // and relations; the header only needs the card.
      (await api.GET("/api/v1/tasks/{id}", { params: { path: { id: taskId } } }))
        .data?.task,
  });
}

/** This ticket's pending asks. The same list the top bar reads, filtered here —
 *  so answering in either place resolves the one row (MAIN-159).
 *
 *  `id` MUST be the ticket's uuid, not the route param: `Interaction.task_id` is
 *  a uuid, and `live.ts` invalidates `["interactions", "task", <uuid>]`. Keyed
 *  or filtered on a board key, this renders nothing and never refreshes — which
 *  is exactly what the board-menu entry path did. `undefined` until the ticket
 *  resolves, which also holds the query. */
function useAsks(id: string | undefined) {
  const { data } = useQuery({
    queryKey: ["interactions", "task", id ?? "unresolved"],
    queryFn: async () => (await api.GET("/api/v1/interactions")).data ?? [],
    enabled: !!id,
  });
  return (data ?? []).filter((i) => i.task_id === id);
}

/**
 * One transcript entry.
 *
 * Three shapes, because three different things arrive on this channel: a
 * drafted issue (render the markdown — it is the thing the human has to read
 * and approve), a human/system turn (short prose), and agent narration (raw PTY
 * output, ANSI stripped, kept monospace and muted so it reads as the background
 * hum it is).
 */
function Entry({ line }: { line: LoopJobTranscriptEntry }) {
  const text = stripAnsi(line.content);
  const draft = looksLikeDraft(line.content);
  return (
    <div
      className={`lw-entry lw-${line.source}${draft ? " lw-draft" : ""}`}
      data-testid={`entry-${line.id}`}
    >
      <div className="lw-entry-head">
        <span className="lw-src">{draft ? "draft" : line.source}</span>
        <span className="faint small">
          {new Date(line.at).toLocaleTimeString()}
        </span>
      </div>
      {draft ? (
        <div className="lw-draft-body" data-testid="draft-body">
          <Markdown src={text} />
        </div>
      ) : (
        <pre className="lw-entry-body">{text}</pre>
      )}
    </div>
  );
}

/** The bottom bar in `seed` mode: the opening idea, then start (AC-2). */
function SeedComposer({
  taskId,
  taskType,
  jobs,
}: {
  taskId: string;
  taskType: string | null | undefined;
  jobs: LoopJob[] | undefined;
}) {
  const qc = useQueryClient();
  const [seed, setSeed] = useState("");
  const { kind, label, disabled, reason } = loopAction(taskType, jobs);

  const start = useMutation({
    mutationFn: () => createLoopJob(kind, taskId, seed),
    onSuccess: () => {
      setSeed("");
      qc.invalidateQueries({ queryKey: taskJobsKey(taskId) });
    },
  });

  const blocked = disabled || start.isPending;
  return (
    <div className="lw-composer" data-testid="composer-seed">
      <label className="lw-seed-label" htmlFor="lw-seed">
        The idea — what do you want out of this run?
      </label>
      <div className="lw-composer-row">
        <textarea
          id="lw-seed"
          className="lw-seed"
          rows={3}
          placeholder={
            kind === "decompose"
              ? "e.g. break this down back-to-front; the API slice first"
              : "e.g. focus on the migration path, not the UI"
          }
          value={seed}
          disabled={blocked}
          onChange={(e) => setSeed(e.target.value)}
          onKeyDown={(e) => {
            // Enter sends, Shift+Enter is a newline — a multi-line brief is
            // normal here, so the modifier is the one that submits.
            if (e.key === "Enter" && (e.metaKey || e.ctrlKey) && !blocked) {
              e.preventDefault();
              start.mutate();
            }
          }}
        />
        <button
          className="btn primary"
          disabled={blocked}
          title={reason ?? label}
          onClick={() => !blocked && start.mutate()}
        >
          <Play size={12} /> {label}
        </button>
      </div>
      <div className="faint small">
        Optional — start with an empty box and the run reads the ticket alone.
      </div>
    </div>
  );
}

/** The bottom bar in `steer` mode: say something the agent never asked for
 *  (AC-3). A message to a paused job resumes it, server-side. */
function MessageComposer({ taskId, jobId }: { taskId: string; jobId: string }) {
  const qc = useQueryClient();
  const [text, setText] = useState("");

  const send = useMutation({
    mutationFn: () => postJobMessage(jobId, text),
    onSuccess: () => {
      setText("");
      qc.invalidateQueries({ queryKey: jobKey(jobId) });
      qc.invalidateQueries({ queryKey: taskJobsKey(taskId) });
    },
  });

  const empty = !text.trim();
  return (
    <div className="lw-composer" data-testid="composer-steer">
      <div className="lw-composer-row">
        <textarea
          className="lw-msg"
          rows={2}
          aria-label="message the agent"
          placeholder="tell the agent something — scope, a correction, go ahead…"
          value={text}
          disabled={send.isPending}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              if (!empty && !send.isPending) send.mutate();
            }
          }}
        />
        <button
          className="btn primary"
          disabled={empty || send.isPending}
          onClick={() => send.mutate()}
          title="send to the run"
          aria-label="send message"
        >
          <Send size={12} />
        </button>
      </div>
      <div className="faint small">
        Enter sends · Shift+Enter for a newline. A message to a paused run
        resumes it.
      </div>
    </div>
  );
}

/** The bottom bar in `readonly` mode: the run is over. Say what happened and
 *  offer the only thing that can still be done (AC-5). */
function ClosedComposer({
  taskId,
  job,
}: {
  taskId: string;
  job: LoopJob;
}) {
  const qc = useQueryClient();
  const rerun = useMutation({
    mutationFn: () =>
      api.POST("/api/v1/jobs/{id}/rerun", { params: { path: { id: job.id } } }),
    onSuccess: () => qc.invalidateQueries({ queryKey: taskJobsKey(taskId) }),
  });
  const failed = job.state === "failed";
  return (
    <div className="lw-composer lw-closed" data-testid="composer-readonly">
      <span className="faint small">
        {failed
          ? "This run failed — the transcript above is the record. Re-run starts a fresh job from the same brief."
          : `This run is ${job.state}. The transcript is read-only; start a new run from the ticket.`}
      </span>
      {(failed || job.state === "canceled") && (
        <button
          className="btn small primary"
          disabled={rerun.isPending}
          onClick={() => rerun.mutate()}
          title="run this job again"
        >
          <RotateCcw size={11} /> Re-run
        </button>
      )}
    </div>
  );
}

/** The full-page Loop workspace. */
export function LoopPage() {
  // The route param is whatever the caller had — the board menu navigates by
  // KEY, the modal by uuid, and both are legal (the server resolves either).
  // Everything downstream keys on the resolved UUID, because that is what the
  // interaction rows carry and what `live.ts` invalidates: keyed on a board key,
  // the asks never match and the jobs list never hears `job_changed`, so the
  // composer sits in a stale mode. Resolve first, then key on `id`.
  const { taskId: routeParam = "" } = useParams();
  const qc = useQueryClient();
  const { data: task } = useTask(routeParam);
  const taskId = task?.id;
  const asks = useAsks(taskId);

  const { data: jobs, isLoading } = useQuery({
    queryKey: taskJobsKey(taskId ?? "unresolved"),
    queryFn: () => fetchTaskJobs(taskId!),
    enabled: !!taskId,
  });

  const latest = jobs && jobs.length > 0 ? jobs[0] : null;

  const { data: detail } = useQuery({
    queryKey: jobKey(latest?.id ?? "none"),
    queryFn: async () =>
      (
        await api.GET("/api/v1/jobs/{id}", {
          params: { path: { id: latest!.id } },
        })
      ).data,
    enabled: !!latest,
  });

  const cancel = useMutation({
    mutationFn: () =>
      api.POST("/api/v1/jobs/{id}/cancel", {
        params: { path: { id: latest!.id } },
      }),
    onSuccess: () => {
      if (taskId) qc.invalidateQueries({ queryKey: taskJobsKey(taskId) });
      if (latest) qc.invalidateQueries({ queryKey: jobKey(latest.id) });
    },
  });

  // Follow the tail as the run streams, but only when the reader is already at
  // the bottom — yanking the view while someone reads back through a draft is
  // worse than not following at all.
  const scroller = useRef<HTMLDivElement | null>(null);
  const transcript = detail?.transcript ?? [];
  useEffect(() => {
    const el = scroller.current;
    if (!el) return;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
    if (atBottom) el.scrollTop = el.scrollHeight;
  }, [transcript.length]);

  const mode = composerMode(latest);
  const meta = latest ? jobStateMeta(latest.state) : null;
  const filed = filedKeys(transcript, task?.key ?? null);

  return (
    <div className="lw" data-testid="loop-workspace">
      <div className="lw-head">
        <Link className="btn small" to="/board" title="back to the board">
          <ArrowLeft size={12} /> Board
        </Link>
        <div className="lw-title">
          <span className="lw-key">{task?.key ?? routeParam}</span>
          <span className="lw-task-title">{task?.title ?? ""}</span>
        </div>
        {meta && (
          <span className={`pill ${meta.tone === "muted" ? "" : meta.tone}`}>
            {meta.label}
          </span>
        )}
        {latest && <span className="faint small">{latest.kind}</span>}
        {latest?.queued_reason && latest.state === "queued" && (
          <span className="small lw-queued" title={latest.queued_reason}>
            {latest.queued_reason}
          </span>
        )}
        <span className="lw-head-actions">
          {filed.map((key) => (
            <Link
              key={key}
              className="btn small"
              to={`/board?task=${key}`}
              title={`open ${key}`}
            >
              <ExternalLink size={11} /> {key}
            </Link>
          ))}
          {mode === "steer" && (
            <button
              className="btn small"
              disabled={cancel.isPending}
              onClick={() => cancel.mutate()}
              title="cancel this loop job"
            >
              <Ban size={11} /> Cancel
            </button>
          )}
        </span>
      </div>

      <div className="lw-body">
        <Panel title="transcript" className="lw-panel">
          <div className="lw-scroll" ref={scroller} data-testid="transcript">
            {!latest ? (
              isLoading ? (
                <div className="faint small">Loading…</div>
              ) : (
                <Empty>
                  No run yet — start a spec or decompose below, and say what you
                  want out of it.
                </Empty>
              )
            ) : transcript.length === 0 ? (
              <div className="faint small lw-quiet">
                Nothing yet — the transcript fills in as the agent works.
              </div>
            ) : (
              transcript.map((l) => <Entry key={l.id} line={l} />)
            )}
          </div>
        </Panel>

        {/* The agent stopped to ask. Its answer controls belong in the flow of
            the conversation, not in a panel somewhere else on the page. */}
        {asks.length > 0 && (
          <div className="lw-asks" data-testid="asks">
            <div className="lw-asks-h">
              The agent is waiting on you · {asks.length}
            </div>
            {asks.map((ixn) => (
              <InteractionAnswer key={ixn.id} interaction={ixn} />
            ))}
          </div>
        )}
      </div>

      <div className="lw-foot">
        {taskId && mode === "seed" && (
          <SeedComposer taskId={taskId} taskType={task?.type} jobs={jobs} />
        )}
        {taskId && mode === "steer" && latest && (
          <MessageComposer taskId={taskId} jobId={latest.id} />
        )}
        {taskId && mode === "readonly" && latest && (
          <ClosedComposer taskId={taskId} job={latest} />
        )}
      </div>
    </div>
  );
}
