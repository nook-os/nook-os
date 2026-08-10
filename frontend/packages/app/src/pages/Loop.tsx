// The Loop workspace (MAIN-233): a full page for driving one ticket's loop job.
//
// The compact panel in the ticket modal (MAIN-128) is a status light — 420px,
// no composer, "no transcript yet" and nowhere to type. Speccing needs room, so
// this is the same job given the whole screen.
//
// The conversation IS the shared `ChatView` (MAIN-299) — the same component team
// chat and `LoopPanel` render. It used to be a bespoke `Entry` row plus three
// bespoke composers, which meant the loop's main surface looked like a different
// product from its own panel and every chat improvement had to be built twice.
// What is left here is the chrome that is genuinely loop-specific:
//
// - `seed` — before there is a job, the opening idea you want the agent to start
//   from (MAIN-231) rather than a bare Play button. Its own control, below the
//   log, because "start a run" is not a message to anyone.
// - `steer` — a live run. ChatView's own composer, posting steering messages;
//   with a question outstanding the same box answers it instead (AC-3).
// - `readonly` — the job is terminal, so there is no session left to talk to.
//   Say so, and offer the re-run, instead of a box that could only fail.
//
// Everything refetches on the live `job_changed` event, so a run streams in
// without a reload.
import React, { useState } from "react";
import { Link, useParams } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ArrowLeft, Ban, ExternalLink, RotateCcw } from "lucide-react";
import { api, type LoopJob, type LoopJobTranscriptEntry } from "@nookos/api";
import { ChatView, Empty, Panel, type ChatViewMessage } from "@nookos/ui";
import type { StuckCause } from "../loop";
import {
  agentActivityLabel,
  composerMode,
  createLoopJob,
  fetchTaskJobs,
  filedKeys,
  jobKey,
  jobStateMeta,
  foldToolActivity,
  loopAction,
  postJobMessage,
  stuckCause,
  taskJobsKey,
} from "../loop";
import { answerInteraction } from "../Interactions";
import { transcriptMessages } from "../LoopPanel";
import { fileSlug, TranscriptActions } from "../transcriptExport";

/** The ticket this workspace is anchored to. Keys and uuids both resolve
 *  server-side (MAIN-209), so the route accepts whichever the caller had. */
function useTask(taskId: string) {
  return useQuery({
    queryKey: ["task", taskId],
    queryFn: async () => {
      // The detail endpoint wraps the card in `.task` alongside its comments
      // and relations; the header only needs the card.
      const res = await api.GET("/api/v1/tasks/{id}", {
        params: { path: { id: taskId } },
      });
      const task = res.data?.task;
      if (task) return task;

      // `null`, never `undefined`. React Query rejects an `undefined` result
      // outright ("Query data cannot be undefined") and leaves `data`
      // undefined — indistinguishable here from "still loading", which is what
      // made a wrong or cross-tenant id render a dead page: the composer is
      // gated on a resolved id, so it vanished, while the transcript fell
      // through to "No run yet". `null` is a RESULT the page can render.
      const status = res.response?.status;
      // A ticket in another tenant answers 404, not 403 — no existence oracle —
      // so both mean the same thing to a reader: it is not yours to see.
      if (status === undefined || status === 404 || status === 403) return null;
      // Anything else is a real failure and must not be dressed up as "no such
      // ticket": React Query marks the query errored and the page says so.
      throw new Error(`could not load ${taskId} (HTTP ${status})`);
    },
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
 * The run, as chat messages (MAIN-299 AC-1/AC-3).
 *
 * This page used to render its own `Entry` rows while `LoopPanel` rendered the
 * same data through the shared `ChatView`, so the loop's main surface and the
 * loop's panel looked like two different products and every chat improvement had
 * to be built twice. One mapping now feeds the one component.
 *
 * Three things the mapping carries that a plain `content` string does not:
 *
 * - **source is the author.** `system` / `agent` / `human` group exactly as
 *   people do in team chat, so a run of narration collapses under one header.
 *   Same rule as `LoopPanel.transcriptMessages`.
 * - **a drafted issue is a document.** It is the thing a human has to read and
 *   approve — a whole spec in markdown — so it renders as markdown rather than
 *   as a wall of literal `##`. That is what `ChatViewMessage.markdown` is for.
 * - **a pending ask is part of the conversation.** The agent stopping to ask is
 *   a turn in the stream, not a strip bolted to the side of it (AC-3), and its
 *   answer goes back through the same composer everything else does.
 */
export function loopMessages(
  transcript: LoopJobTranscriptEntry[],
  asks: { id: string; prompt: string }[] = [],
): ChatViewMessage[] {
  // One mapping, shared with the Loop panel and the workspace Reviews panel —
  // this file carried its own copy and the shared one drifted behind it, which
  // is how the same transcript rendered differently on two surfaces.
  const lines = transcriptMessages(foldToolActivity(transcript));
  // Appended, not interleaved: an ask is outstanding *now*, so it belongs at the
  // bottom where the reader is, next to the box they answer it in.
  return [
    ...lines,
    ...asks.map((a) => ({
      id: `ask-${a.id}`,
      authorId: "agent",
      authorName: "agent",
      body: a.prompt,
      createdAt: new Date().toISOString(),
    })),
  ];
}

/** The tenant's loop master switch (MAIN-239), read here so a stuck run can say
 *  the switch is the reason. Shares `["settings"]` with the Settings page, so
 *  turning it on there repaints this without a reload. */
function useLoopsEnabled(): boolean | undefined {
  const { data } = useQuery({
    queryKey: ["settings"],
    queryFn: async () => (await api.GET("/api/v1/settings")).data ?? [],
  });
  // `undefined` until the query answers — "not loaded" must not read as "off".
  if (!data) return undefined;
  return (
    data.find((x) => x.key === "loops.enabled" && x.scope === "tenant")
      ?.value === true
  );
}

/**
 * Why this run is not moving, and the fix, where the problem appears (MAIN-297).
 *
 * The whole point is that the fix is one click from the stuck run: both causes
 * are fixed on OTHER pages, which is what made this a dead end — a run blocked
 * by `loops.enabled=false` is only unblockable from Settings→Loops, and nothing
 * on the run said so.
 */
function StuckNotice({ cause }: { cause: StuckCause }) {
  const qc = useQueryClient();
  const [failed, setFailed] = useState<string | null>(null);

  const turnOn = useMutation({
    mutationFn: async () => {
      const res = await api.PUT("/api/v1/settings/{key}", {
        params: { path: { key: "loops.enabled" } },
        body: { value: true, scope: "tenant" },
      });
      // openapi-fetch reports HTTP failures in `error` rather than throwing, so
      // without this a refused write would look like a success and the notice
      // would simply sit there (AC-4).
      if (res.error) throw new Error("could not turn loops on");
      return res;
    },
    onSuccess: () => {
      setFailed(null);
      // The switch is read on every poll, so the dispatcher picks the job up
      // within one interval and `job_changed` repaints this page. Invalidating
      // both means the notice clears as soon as either lands (AC-2).
      qc.invalidateQueries({ queryKey: ["settings"] });
      qc.invalidateQueries({ queryKey: ["jobs"] });
    },
    onError: () =>
      setFailed(
        "Could not turn loops on — you may not have permission. Ask a workspace owner, or use Settings → Loops.",
      ),
  });

  if (cause.kind === "waiting") {
    return (
      <div className="lw-stuck" data-testid="stuck-waiting">
        <span className="faint small">
          {cause.detail ?? "Waiting for an executor…"}
        </span>
      </div>
    );
  }

  if (cause.kind === "no-executor") {
    return (
      <div className="lw-stuck" data-testid="stuck-no-executor">
        <div className="bright">This run has nowhere to go.</div>
        {/* The backend already distinguishes "no node online" from "not
            authorized"; repeating its sentence keeps one source of truth. */}
        <div className="faint small">{cause.detail}</div>
        <Link className="btn small" to="/nodes">
          Open Nodes
        </Link>
      </div>
    );
  }

  return (
    <div className="lw-stuck" data-testid="stuck-loops-off">
      <div className="bright">Loops are off, so nothing will run this.</div>
      <div className="faint small">
        Jobs stay queued until the loop machinery is on — nothing is lost. It
        takes effect within a poll interval; you can stay on this page.
      </div>
      <button
        className="btn small primary"
        disabled={turnOn.isPending}
        onClick={() => turnOn.mutate()}
      >
        {turnOn.isPending ? "Turning on…" : "Turn on loops"}
      </button>
      <Link className="btn small" to="/settings">
        Settings → Loops
      </Link>
      {failed && (
        <div className="small err" data-testid="stuck-loops-off-failed">
          {failed}
        </div>
      )}
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
  const {
    data: task,
    isPending: taskPending,
    isError: taskFailed,
  } = useTask(routeParam);
  const taskId = task?.id;
  const asks = useAsks(taskId);
  // Three states, not two. Until this was separated, "we could not find it" and
  // "we have not looked yet" were the same value, and the page rendered the
  // no-run message with no composer under it — a dead box.
  const missing = !taskPending && !taskFailed && task == null;

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

  // Following the tail is ChatView's job now, not this page's — it already
  // follows an append only when the reader is at the bottom, which is the same
  // rule the hand-rolled scroller here implemented (MAIN-299 AC-4).
  const transcript = detail?.transcript ?? [];

  // The one pending ask, if the agent stopped to ask something. It changes what
  // the composer MEANS: with an ask outstanding, what you type is the answer to
  // it; otherwise it is an unprompted steer. Both go out through the same box
  // (AC-3), so the difference lives here rather than in two components.
  const ask = asks[0];

  const send = useMutation({
    mutationFn: async (body: string) => {
      if (ask) return answerInteraction(qc, ask, body);
      if (latest) return postJobMessage(latest.id, body);
    },
    onSuccess: () => {
      if (latest) qc.invalidateQueries({ queryKey: jobKey(latest.id) });
      if (taskId) qc.invalidateQueries({ queryKey: taskJobsKey(taskId) });
    },
  });
  const followUp = useMutation({
    // A completed run is done, but the conversation isn't: sending starts a
    // follow-up run seeded with the message. It becomes the newest job (jobs[0])
    // and the page follows it, so the thread continues without the reader ever
    // touching a mode switch — "also add X" just keeps going.
    mutationFn: async (body: string) => {
      if (!taskId) return;
      return createLoopJob(
        latest?.kind === "decompose" ? "decompose" : "spec",
        taskId,
        body,
      );
    },
    onSuccess: () => {
      if (taskId) qc.invalidateQueries({ queryKey: taskJobsKey(taskId) });
    },
  });
  // `seed` mode reuses the SAME ChatView composer instead of a bespoke box:
  // sending it STARTS the run (loopAction picks spec vs decompose off the ticket
  // type). One chat component for every mode — the only difference is what send
  // does, which lives here.
  const seedAction = loopAction(task?.type, jobs);
  const seedStart = useMutation({
    mutationFn: async (body: string) => {
      if (!taskId) return;
      return createLoopJob(seedAction.kind, taskId, body);
    },
    onSuccess: () => {
      if (taskId) qc.invalidateQueries({ queryKey: taskJobsKey(taskId) });
    },
  });
  const sending = send.isPending || followUp.isPending || seedStart.isPending;
  const onSend = (body: string) => {
    const m = composerMode(latest);
    if (m === "seed") return seedStart.mutate(body);
    if (m === "continue") return followUp.mutate(body);
    return send.mutate(body);
  };

  // Why a queued run is not moving, and where its fix lives (MAIN-297).
  const loopsEnabled = useLoopsEnabled();
  const stuck = stuckCause(latest, loopsEnabled);
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
          {latest && (
            <TranscriptActions
              // The FULL transcript (AC-3) — the page's fold is display-only.
              lines={transcript}
              filename={`${fileSlug(task?.key ?? routeParam ?? "task")}-${latest.id.slice(0, 8)}.md`}
            />
          )}
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
          {missing || taskFailed ? (
            // Say which of the two it is. "Not found" for a ticket that is
            // genuinely not visible; a load failure is a different problem
            // and must not be reported as a missing ticket.
            <Empty>
              <span data-testid="loop-not-found">
                {taskFailed
                  ? `Could not load ${routeParam}.`
                  : `${routeParam} doesn't exist, or isn't in your workspace.`}
              </span>{" "}
              <Link to="/board">Back to the board</Link>
            </Empty>
          ) : (
            // ONE chat surface, the same component team chat and the loop panel
            // render (MAIN-299). It owns the scroll and the follow-the-bottom
            // behaviour, which is why the bespoke `lw-scroll` ref went with the
            // bespoke rows.
            //
            // The composer is ChatView's ONLY in `steer` mode, because that is
            // the only mode where there is a live run to talk to. Seeding a run
            // and reporting a finished one are loop-specific chrome, so they
            // stay their own controls below (AC-2).
            <div
              className="lw-chat"
              data-testid={mode === "readonly" ? "transcript" : `composer-${mode}`}
            >
              <ChatView
                variant="transcript"
                messages={loopMessages(transcript, asks)}
                onSend={onSend}
                hideComposer={mode === "readonly" || !taskId}
                disabled={sending || (mode === "seed" && seedAction.disabled)}
                sendLabel={mode === "seed" ? seedAction.label : "Send"}
                allowEmpty={mode === "seed"}
                placeholder={
                  ask
                    ? "Answer the agent…"
                    : mode === "seed"
                      ? seedAction.kind === "decompose"
                        ? "e.g. break this down back-to-front; the API slice first"
                        : "e.g. focus on the migration path, not the UI"
                      : mode === "continue"
                        ? "ask a follow-up — refine the spec, add a requirement…"
                        : "tell the agent something — scope, a correction, go ahead…"
                }
                emptyLabel={
                  latest
                    ? "Nothing yet — the transcript fills in as the agent works."
                    : isLoading || taskPending
                      ? "Loading…"
                      : "No run yet — say what you want out of it in the box below, or send it empty to read the ticket alone."
                }
                typing={
                  latest && mode === "steer"
                    ? agentActivityLabel(latest.state)
                    : null
                }
                beforeComposer={
                  // The ask's CHOICES only. Its prompt is already in the stream
                  // as a message (AC-3), so repeating it here would say the same
                  // thing twice; what a reader still needs is the buttons.
                  ask && (ask.choices ?? []).length > 0 ? (
                    <div className="lw-ask-choices" data-testid="ask-choices">
                      {(ask.choices ?? []).map((c) => (
                        <button
                          key={c}
                          className="btn small"
                          disabled={sending}
                          onClick={() => void onSend(c)}
                        >
                          {c}
                        </button>
                      ))}
                    </div>
                  ) : null
                }
              />
            </div>
          )}
        </Panel>

        {/* A queued run that cannot be placed says WHY, and offers the fix
            here rather than on a page the reader would have to guess at. */}
        {stuck && <StuckNotice cause={stuck} />}
      </div>

      {/* The footer is now ONLY the readonly (failed/canceled) closer — a re-run
          affordance for a dead-end run. Every interactive mode — seed, steer,
          continue — uses ChatView's own composer in the panel above: one shared
          box, one component, everywhere. */}
      <div
        className="lw-foot"
        data-testid="loop-foot"
        hidden={!taskId || mode !== "readonly"}
      >
        {taskId && mode === "readonly" && latest && (
          <ClosedComposer taskId={taskId} job={latest} />
        )}
      </div>
    </div>
  );
}
