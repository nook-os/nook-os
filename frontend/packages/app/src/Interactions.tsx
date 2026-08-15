// Durable human interactions (MAIN-159).
//
// An executor — an agent on some node, mid-run — sometimes has to stop and ask
// a person a question it cannot answer itself: "which of these branches?",
// "proceed?". That ask used to live only in the terminal that raised it, so it
// died with the tab. A durable interaction outlives the tab: it is a row on the
// control plane, listed here in a top-bar indicator (mirroring the bell) and,
// when the ask names a ticket, inline on that ticket. Answering it from either
// place resolves the same row, and the websocket pushes the change everywhere.
import React, { useState } from "react";
import { Link } from "react-router-dom";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { MessageCircleQuestion, Send, X } from "lucide-react";
import { api, type Interaction, type LoopJobTranscriptEntry } from "@nookos/api";
import { shortAge } from "./QueuePanel";

/** The pending list is the one query both surfaces read; keeping the key here
 *  means the live-event invalidation and every caller agree on it. */
/// The pending-asks cache. Exported because the browser-tab title reads it
/// (MAIN-450) — reading the key beats re-typing the array, which is how two
/// copies of one cache key start disagreeing.
export const PENDING_KEY = ["interactions", "pending"] as const;

async function fetchPending(): Promise<Interaction[]> {
  return (await api.GET("/api/v1/interactions")).data ?? [];
}

/**
 * The answer controls, shared by the top-bar panel and the per-ticket surface
 * so the two can never drift: a button per structured choice (when the ask
 * offered any), and always a free-text box with Send. Answering posts the
 * response and invalidates the pending list; the websocket then refreshes the
 * other surface.
 */
export function InteractionAnswer({
  interaction,
  onAnswered,
}: {
  interaction: Interaction;
  /** Called once the answer landed — the modal closes on it (AC-8). Absent on
   *  the inline surfaces, which simply lose the item when the list refetches. */
  onAnswered?: () => void;
}) {
  const qc = useQueryClient();
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);

  const answer = async (response: string) => {
    const trimmed = response.trim();
    if (!trimmed || busy) return;
    setBusy(true);
    try {
      await api.POST("/api/v1/interactions/{id}/answer", {
        params: { path: { id: interaction.id } },
        body: { response: trimmed },
      });
      setText("");
      // The pending list is the source of truth; the ticket-scoped copy is a
      // filter over it, so invalidating both keeps them in step immediately —
      // the websocket echo only confirms what we already showed.
      qc.invalidateQueries({ queryKey: PENDING_KEY });
      if (interaction.task_id) {
        qc.invalidateQueries({
          queryKey: ["interactions", "task", interaction.task_id],
        });
      }
      onAnswered?.();
    } finally {
      setBusy(false);
    }
  };

  const choices = interaction.choices ?? [];

  return (
    <div className="ixn-item">
      <div className="ixn-prompt">{interaction.prompt}</div>
      {choices.length > 0 && (
        <div className="ixn-choices">
          {choices.map((c) => (
            <button
              key={c}
              className="btn small"
              disabled={busy}
              onClick={() => void answer(c)}
            >
              {c}
            </button>
          ))}
        </div>
      )}
      <div className="ixn-reply">
        <input
          className="ixn-input"
          placeholder="type a reply…"
          aria-label="reply"
          value={text}
          disabled={busy}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              void answer(text);
            }
          }}
        />
        <button
          className="btn small primary"
          disabled={busy || !text.trim()}
          onClick={() => void answer(text)}
          title="send reply"
          aria-label="send reply"
        >
          <Send size={11} />
        </button>
      </div>
    </div>
  );
}

/** How much of a run's transcript the modal shows: enough to see what the agent
 *  was doing when it stopped, not a log reader. The runs view is one click away
 *  for the rest. */
const TRANSCRIPT_TAIL = 8;

/** How long this has been waiting — the fact that decides whether it is urgent,
 *  and the one thing a prompt on its own never says. */
export function waitedFor(createdAt: string, now: number): string | null {
  const at = Date.parse(createdAt);
  return Number.isNaN(at) ? null : shortAge(Math.max(0, now - at));
}

/** The end of the run's transcript, through the endpoint the runs view already
 *  reads (AC-10) — same path, same `["job", id]` cache key, so opening this
 *  after looking at the run costs nothing and no second endpoint exists to
 *  drift from it. */
function RunTail({ jobId }: { jobId: string }) {
  const { data } = useQuery({
    queryKey: ["job", jobId],
    queryFn: async () =>
      (await api.GET("/api/v1/jobs/{id}", { params: { path: { id: jobId } } })).data as
        | { transcript?: LoopJobTranscriptEntry[] }
        | undefined,
  });
  const lines = (data?.transcript ?? []).slice(-TRANSCRIPT_TAIL);
  return (
    <div className="ixn-tail" data-testid="ixn-transcript">
      <div className="ixn-context-h small faint">what the run was doing</div>
      {lines.length === 0 ? (
        <div className="small faint">No transcript yet for this run.</div>
      ) : (
        lines.map((l) => (
          <div key={l.id} className="ixn-tail-line small mono">
            <span className="faint">{l.source}</span> {l.content}
          </div>
        ))
      )}
    </div>
  );
}

/**
 * One ask, full size (MAIN-600 AC-8).
 *
 * A prompt with no context is unanswerable: "which branch?" needs the card that
 * asked, the run that asked it, and how long it has been stuck. Every one of
 * those comes off the `Interaction` itself — there is no new field and no new
 * endpoint here (NG-1/NG-6), which is also why each is rendered only when the
 * ask actually carries it. A standalone ask carrying neither is a supported
 * shape and still answers (AC-12).
 */
export function InteractionModal({
  interaction,
  onClose,
}: {
  interaction: Interaction;
  onClose: () => void;
}) {
  const waited = waitedFor(interaction.created_at, Date.now());
  return (
    <div className="modal-backdrop" onMouseDown={onClose}>
      <div
        className="modal"
        style={{ width: 640, maxHeight: "80vh", overflowY: "auto" }}
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={(e) => e.key === "Escape" && onClose()}
      >
        <div className="modal-header">
          A run is waiting on you
          <button
            className="btn small icon"
            onClick={onClose}
            title="close"
            aria-label="close"
            style={{ marginLeft: "auto" }}
          >
            <X size={12} />
          </button>
        </div>
        <div className="modal-body">
          <div className="ixn-context small" data-testid="ixn-context">
            {waited && <span>waiting {waited}</span>}
            {interaction.task_id && (
              <Link to={`/loop/${interaction.task_id}`} onClick={onClose}>
                open the card
              </Link>
            )}
            {interaction.job_id && (
              <span className="mono faint">run {interaction.job_id.slice(0, 8)}</span>
            )}
          </div>
          {interaction.job_id && <RunTail jobId={interaction.job_id} />}
          {/* The SAME control both other surfaces use (AC-11): the choices and
              the free-text box are one implementation, so an answer typed here
              cannot post differently from one typed on the ticket. */}
          <InteractionAnswer interaction={interaction} onAnswered={onClose} />
        </div>
      </div>
    </div>
  );
}

/** One queued ask: what was asked, and how long it has gone unanswered. */
function PendingRow({
  interaction,
  onOpen,
}: {
  interaction: Interaction;
  onOpen: () => void;
}) {
  const waited = waitedFor(interaction.created_at, Date.now());
  return (
    <button type="button" className="ixn-row" onClick={onOpen}>
      <span className="ixn-prompt">{interaction.prompt}</span>
      {waited && <span className="faint small">waiting {waited}</span>}
    </button>
  );
}

/** The top-bar indicator: a badge with the pending count, opening a panel that
 *  lists every pending ask. Answering happens in the modal a row opens, not in
 *  the panel (AC-8) — the panel is a queue, and a queue row has no room for the
 *  context an answer needs. Mirrors `NotificationBell`. */
export function PendingInteractions() {
  const [open, setOpen] = useState(false);
  const [openId, setOpenId] = useState<string | null>(null);

  const { data } = useQuery({
    queryKey: PENDING_KEY,
    queryFn: fetchPending,
    // The websocket pushes changes; this is only a safety net for a client that
    // reconnected while an ask was raised or answered elsewhere.
    refetchInterval: 120000,
  });

  const pending = data ?? [];
  const count = pending.length;
  // Read out of the live list, so an ask answered on another surface closes
  // this rather than leaving a modal answering a resolved row.
  const showing = pending.find((i) => i.id === openId) ?? null;

  return (
    <div className="bell-host">
      <button
        className={`bell${count > 0 ? " has-unread" : ""}`}
        onClick={() => setOpen((v) => !v)}
        title={count > 0 ? `${count} awaiting an answer` : "no pending questions"}
        aria-label={count > 0 ? `${count} pending interactions` : "pending interactions"}
      >
        <MessageCircleQuestion size={14} />
        {count > 0 && <span className="bell-count">{count > 99 ? "99+" : count}</span>}
      </button>

      {open && (
        <>
          <div className="bell-scrim" onClick={() => setOpen(false)} />
          <div className="bell-panel">
            <div className="bell-head">
              <span className="bright">Questions for you</span>
            </div>
            <div className="bell-list">
              {count === 0 && (
                <div className="faint small" style={{ padding: 12 }}>
                  Nothing waiting. When an agent needs an answer to keep going, it
                  lands here.
                </div>
              )}
              {pending.map((ixn) => (
                <PendingRow
                  key={ixn.id}
                  interaction={ixn}
                  // The panel closes with the click that opens the modal: it is
                  // a queue you picked one item out of, and its own scrim sits
                  // ABOVE every modal layer (`.bell-panel` is z-index 360).
                  onOpen={() => {
                    setOpenId(ixn.id);
                    setOpen(false);
                  }}
                />
              ))}
            </div>
          </div>
        </>
      )}

      {showing && (
        <InteractionModal interaction={showing} onClose={() => setOpenId(null)} />
      )}
    </div>
  );
}

/** Answer a pending interaction from anywhere, without the surrounding
 *  `InteractionAnswer` chrome (MAIN-237): the loop panel routes its reply
 *  through the shared chat composer instead, so the answer POST and the cache
 *  invalidation must live somewhere both can call. Same endpoint, same
 *  invalidations — the two paths cannot drift into answering differently. */
export async function answerInteraction(
  qc: ReturnType<typeof useQueryClient>,
  interaction: Interaction,
  response: string,
): Promise<void> {
  const trimmed = response.trim();
  if (!trimmed) return;
  await api.POST("/api/v1/interactions/{id}/answer", {
    params: { path: { id: interaction.id } },
    body: { response: trimmed },
  });
  qc.invalidateQueries({ queryKey: PENDING_KEY });
  if (interaction.task_id) {
    qc.invalidateQueries({
      queryKey: ["interactions", "task", interaction.task_id],
    });
  }
}

/** The pending asks for one ticket. Exported so the loop panel can drive its
 *  composer from the same list the modal section renders (MAIN-237). */
export function useTaskInteractions(taskId: string): Interaction[] {
  const { data } = useQuery({
    queryKey: ["interactions", "task", taskId],
    queryFn: fetchPending,
  });
  return (data ?? []).filter((ixn) => ixn.task_id === taskId);
}

/** The per-ticket surface (MAIN-159): the pending asks for THIS ticket, inline
 *  in its detail modal. Reads the same pending list and filters to the ticket,
 *  so it shares the top bar's cache and its live invalidation. Renders nothing
 *  when the ticket has no pending ask. */
export function TaskInteractions({ taskId }: { taskId: string }) {
  const mine = useTaskInteractions(taskId);
  if (mine.length === 0) return null;

  return (
    <div className="task-section">
      <div className="task-section-h">questions · {mine.length}</div>
      {mine.map((ixn) => (
        <InteractionAnswer key={ixn.id} interaction={ixn} />
      ))}
    </div>
  );
}
