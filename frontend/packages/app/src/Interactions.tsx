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
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { MessageCircleQuestion, Send } from "lucide-react";
import { api, type Interaction } from "@nookos/api";

/** The pending list is the one query both surfaces read; keeping the key here
 *  means the live-event invalidation and every caller agree on it. */
const PENDING_KEY = ["interactions", "pending"] as const;

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
export function InteractionAnswer({ interaction }: { interaction: Interaction }) {
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

/** The top-bar indicator: a badge with the pending count, opening a panel that
 *  lists every pending ask with its answer controls. Mirrors `NotificationBell`. */
export function PendingInteractions() {
  const [open, setOpen] = useState(false);

  const { data } = useQuery({
    queryKey: PENDING_KEY,
    queryFn: fetchPending,
    // The websocket pushes changes; this is only a safety net for a client that
    // reconnected while an ask was raised or answered elsewhere.
    refetchInterval: 120000,
  });

  const pending = data ?? [];
  const count = pending.length;

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
                <InteractionAnswer key={ixn.id} interaction={ixn} />
              ))}
            </div>
          </div>
        </>
      )}
    </div>
  );
}

/** The per-ticket surface (MAIN-159): the pending asks for THIS ticket, inline
 *  in its detail modal. Reads the same pending list and filters to the ticket,
 *  so it shares the top bar's cache and its live invalidation. Renders nothing
 *  when the ticket has no pending ask. */
export function TaskInteractions({ taskId }: { taskId: string }) {
  const { data } = useQuery({
    queryKey: ["interactions", "task", taskId],
    queryFn: fetchPending,
  });

  const mine = (data ?? []).filter((ixn) => ixn.task_id === taskId);
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
