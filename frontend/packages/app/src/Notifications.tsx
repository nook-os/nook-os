// Toasts and the bell.
//
// The old system played a chime and, if you had granted permission, raised a
// desktop notification. Both are fire-and-forget: if you were looking at
// another window, or the sound was muted, the thing that happened left no
// trace you could go back to. That is the gap this closes — every notification
// lands in an inbox you can open, and the ones that arrive while you are
// looking get a toast as well.
import React, { useEffect, useState } from "react";
import { create } from "zustand";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Bell, Check, CheckCheck, Trash2, X } from "lucide-react";
import { api, setWriteFailureHandler, type Notification } from "@nookos/api";

/** Toasts currently on screen. Separate from the inbox: this is ephemeral. */
interface ToastState {
  toasts: Notification[];
  push(n: Notification): void;
  dismiss(id: string): void;
}

export const useToasts = create<ToastState>((set) => ({
  toasts: [],
  push: (n) =>
    set((s) =>
      // The same notification can arrive twice — a websocket reconnect replays,
      // and an optimistic local raise races the server's copy.
      s.toasts.some((t) => t.id === n.id)
        ? s
        : { toasts: [...s.toasts, n].slice(-4) },
    ),
  dismiss: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),
}));

/**
 * Say so when a write does not happen.
 *
 * Installed once at startup. Nothing else in the app reports these: every call
 * site reads `data` and drops `error`, so a failed save looked exactly like a
 * button that did nothing — which is how the desktop app was able to lose
 * every write it made on macOS without a single screen mentioning it.
 *
 * Local only: this never reaches the inbox, because the inbox lives on the
 * control plane and the whole problem is that we cannot reach it.
 */
export function installWriteFailureToasts(): () => void {
  let n = 0;
  setWriteFailureHandler((f) => {
    useToasts.getState().push({
      // Not a server id — nothing on the server knows about this.
      id: `write-failure-${++n}`,
      tenant_id: "",
      level: "error",
      title: "That change was not saved",
      // The path, because "a task" and "a setting" fail identically otherwise,
      // and the message, because the server usually says something useful.
      body: `${f.method} ${f.path} — ${f.message}`,
      kind: "client.write_failed",
      payload: null,
      created_at: new Date().toISOString(),
    } as Notification);
  });
  return () => setWriteFailureHandler(null);
}

const TONE: Record<string, string> = {
  info: "info",
  success: "ok",
  warning: "warn",
  error: "err",
};

/** Errors stay until dismissed; everything else fades. */
function lifetimeMs(level: string): number | null {
  return level === "error" ? null : level === "warning" ? 12000 : 7000;
}

function Toast({ n }: { n: Notification }) {
  const dismiss = useToasts((s) => s.dismiss);

  useEffect(() => {
    const ms = lifetimeMs(n.level);
    if (ms === null) return;
    const t = setTimeout(() => dismiss(n.id), ms);
    return () => clearTimeout(t);
  }, [n.id, n.level, dismiss]);

  const body = (
    <>
      <div className="toast-title">{n.title}</div>
      {n.body && <div className="toast-body">{n.body}</div>}
    </>
  );

  return (
    <div className={`toast ${TONE[n.level] ?? "info"}`}>
      {n.link ? (
        <a className="toast-main" href={n.link}>
          {body}
        </a>
      ) : (
        <div className="toast-main">{body}</div>
      )}
      <button className="toast-x" onClick={() => dismiss(n.id)} title="dismiss">
        <X size={12} />
      </button>
    </div>
  );
}

export function Toasts() {
  const toasts = useToasts((s) => s.toasts);
  if (toasts.length === 0) return null;
  return (
    <div className="toast-stack">
      {toasts.map((t) => (
        <Toast key={t.id} n={t} />
      ))}
    </div>
  );
}

/** The bell, its unread count, and the inbox behind it. */
export function NotificationBell() {
  const [open, setOpen] = useState(false);
  const qc = useQueryClient();

  const { data } = useQuery({
    queryKey: ["notifications"],
    queryFn: async () => (await api.GET("/api/v1/notifications")).data ?? null,
    // The websocket pushes new ones, so this is only a safety net for a client
    // that reconnected while something was raised.
    refetchInterval: 120000,
  });

  const unread = data?.unread ?? 0;
  const list = data?.notifications ?? [];

  const markAll = async () => {
    await api.POST("/api/v1/notifications/read", { body: {} });
    qc.invalidateQueries({ queryKey: ["notifications"] });
  };
  const markOne = async (id: string) => {
    await api.POST("/api/v1/notifications/read", { body: { id } });
    qc.invalidateQueries({ queryKey: ["notifications"] });
  };
  const clearAll = async () => {
    await api.DELETE("/api/v1/notifications");
    qc.invalidateQueries({ queryKey: ["notifications"] });
  };

  return (
    <div className="bell-host">
      <button
        className={`bell${unread > 0 ? " has-unread" : ""}`}
        onClick={() => setOpen((v) => !v)}
        title={unread > 0 ? `${unread} unread` : "notifications"}
      >
        <Bell size={14} />
        {unread > 0 && <span className="bell-count">{unread > 99 ? "99+" : unread}</span>}
      </button>

      {open && (
        <>
          <div className="bell-scrim" onClick={() => setOpen(false)} />
          <div className="bell-panel">
            <div className="bell-head">
              <span className="bright">Notifications</span>
              <span style={{ display: "inline-flex", gap: 3 }}>
                <button
                  className="btn small"
                  onClick={markAll}
                  disabled={unread === 0}
                  title="mark all read"
                >
                  <CheckCheck size={12} />
                </button>
                <button
                  className="btn small"
                  onClick={clearAll}
                  disabled={list.length === 0}
                  title="clear"
                >
                  <Trash2 size={11} />
                </button>
              </span>
            </div>

            <div className="bell-list">
              {list.length === 0 && (
                <div className="faint small" style={{ padding: 12 }}>
                  Nothing yet. Things that happen across your fleet land here.
                </div>
              )}
              {list.map((n) => (
                <div key={n.id} className={`bell-item ${TONE[n.level] ?? "info"}${n.read_at ? "" : " unread"}`}>
                  <div className="bell-item-main">
                    <div className="bell-item-title">
                      {n.link ? <a href={n.link}>{n.title}</a> : n.title}
                    </div>
                    {n.body && <div className="bell-item-body">{n.body}</div>}
                    <div className="faint small">
                      {new Date(n.created_at).toLocaleString()}
                      {n.kind !== "custom" && ` · ${n.kind}`}
                    </div>
                  </div>
                  {!n.read_at && (
                    <button
                      className="btn small"
                      onClick={() => markOne(n.id)}
                      title="mark read"
                    >
                      <Check size={11} />
                    </button>
                  )}
                </div>
              ))}
            </div>
          </div>
        </>
      )}
    </div>
  );
}
