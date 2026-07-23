// Where notifications go besides this browser.
//
// The form is built from `/notification-channels/kinds` rather than from a
// copy of the provider list in here. That is the whole reason the server
// describes its own fields: adding Discord on the backend makes it appear in
// this UI without a frontend change, and a frontend that had its own list
// would be a second place to forget.
import React, { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { BellRing, Check, Plus, Send, Trash2, TriangleAlert } from "lucide-react";
import { api, type ChannelKind, type NotificationChannel } from "@nookos/api";
import { Panel, Select } from "@nookos/ui";
import { askConfirm, notify } from "./dialogs";

const LEVELS = ["info", "success", "warning", "error"];

export function NotificationChannels() {
  const qc = useQueryClient();
  const [adding, setAdding] = useState(false);
  const [kind, setKind] = useState("ntfy");
  const [name, setName] = useState("");
  const [config, setConfig] = useState<Record<string, string>>({});
  const [levels, setLevels] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);

  const { data: kinds } = useQuery({
    queryKey: ["channel-kinds"],
    queryFn: async () =>
      (await api.GET("/api/v1/notification-channels/kinds")).data ?? [],
  });
  const { data: channels } = useQuery({
    queryKey: ["channels"],
    queryFn: async () => (await api.GET("/api/v1/notification-channels")).data ?? [],
  });

  const spec: ChannelKind | undefined = (kinds ?? []).find((k) => k.id === kind);
  const bust = () => qc.invalidateQueries({ queryKey: ["channels"] });

  const create = async () => {
    setBusy(true);
    const { error } = await api.POST("/api/v1/notification-channels", {
      body: {
        kind,
        name: name.trim() || spec?.label || kind,
        config,
        levels,
        kinds: [],
      },
    });
    setBusy(false);
    if (error) {
      // The server's own message: it names the missing field, or says the URL
      // points inside the network — both of which are fixable right here.
      await notify("Could not add that channel", messageOf(error));
      return;
    }
    setAdding(false);
    setName("");
    setConfig({});
    setLevels([]);
    bust();
  };

  const test = async (c: NotificationChannel) => {
    const { error } = await api.POST("/api/v1/notification-channels/{id}/test", {
      params: { path: { id: c.id } },
    });
    await notify(
      error ? `${c.name} did not work` : `${c.name} works`,
      error ? messageOf(error) : "A test notification was delivered.",
    );
    bust();
  };

  const toggle = async (c: NotificationChannel) => {
    await api.PATCH("/api/v1/notification-channels/{id}", {
      params: { path: { id: c.id } },
      body: { enabled: !c.enabled },
    });
    bust();
  };

  const remove = async (c: NotificationChannel) => {
    const ok = await askConfirm({
      title: `Remove ${c.name}`,
      description: "Notifications will stop being delivered there.",
      confirmLabel: "remove",
      danger: true,
    });
    if (!ok) return;
    await api.DELETE("/api/v1/notification-channels/{id}", {
      params: { path: { id: c.id } },
    });
    bust();
  };

  return (
    <Panel
      title="Notification channels"
      actions={
        !adding && (
          <button className="btn small" onClick={() => setAdding(true)}>
            <Plus size={12} /> add channel
          </button>
        )
      }
    >
      <div className="chan-wrap">
        <p className="muted small chan-intro">
          Where notifications go besides this browser. Everything that happens on
          your fleet — and anything <code className="mono">nook notify</code> sends,
          including an agent finishing — is delivered to every channel whose
          filters match.
        </p>

        {adding && (
          <div className="chan-form">
            <div className="chan-row">
              <span className="faint small">Type</span>
              <Select
                value={kind}
                onChange={(v) => {
                  setKind(v);
                  setConfig({});
                }}
                options={(kinds ?? []).map((k) => ({ value: k.id, label: k.label }))}
              />
            </div>
            {spec && <p className="muted small">{spec.description}</p>}

            <div className="chan-row">
              <span className="faint small">Name</span>
              <input
                className="chan-input"
                value={name}
                placeholder={spec?.label ?? "my channel"}
                onChange={(e) => setName(e.target.value)}
              />
            </div>

            {(spec?.fields ?? []).map((f) => (
              <div className="chan-row" key={f.name}>
                <span className="faint small">{f.label}</span>
                <input
                  className="chan-input"
                  type={f.secret ? "password" : "text"}
                  value={config[f.name] ?? ""}
                  placeholder={f.placeholder}
                  autoComplete="off"
                  onChange={(e) =>
                    setConfig((c) => ({ ...c, [f.name]: e.target.value }))
                  }
                />
              </div>
            ))}

            <div className="chan-row">
              <span className="faint small">Only</span>
              <div className="chan-levels">
                {LEVELS.map((l) => (
                  <button
                    key={l}
                    className={`task-chip ${levels.includes(l) ? "on" : ""}`}
                    onClick={() =>
                      setLevels((s) =>
                        s.includes(l) ? s.filter((x) => x !== l) : [...s, l],
                      )
                    }
                  >
                    {l}
                  </button>
                ))}
                <span className="faint small">
                  {levels.length === 0 ? "everything" : ""}
                </span>
              </div>
            </div>

            <div className="chan-actions">
              <button className="btn small" onClick={() => setAdding(false)}>
                cancel
              </button>
              <button className="btn small primary" onClick={create} disabled={busy}>
                {busy ? "adding…" : "add channel"}
              </button>
            </div>
          </div>
        )}

        {(channels ?? []).length === 0 && !adding && (
          <div className="faint small chan-empty">
            <BellRing size={13} /> No channels yet. Notifications still appear in
            the bell — a channel is for hearing about them when you are not here.
          </div>
        )}

        {(channels ?? []).map((c) => (
          <div key={c.id} className={`chan-item${c.enabled ? "" : " off"}`}>
            <div className="chan-item-main">
              <div>
                <span className="bright">{c.name}</span>{" "}
                <span className="faint small mono">{c.kind}</span>
              </div>
              <div className="faint small">
                {c.levels.length > 0 ? c.levels.join(", ") : "all levels"}
                {c.kinds.length > 0 && ` · ${c.kinds.join(", ")}`}
                {c.last_ok_at && ` · last delivered ${new Date(c.last_ok_at).toLocaleString()}`}
              </div>
              {/* A channel that has quietly been failing is the failure mode
                  worth surfacing: it looks identical to one with nothing to
                  say. */}
              {c.last_error && (
                <div className="chan-error small">
                  <TriangleAlert size={11} /> {c.last_error}
                </div>
              )}
            </div>
            <div className="chan-item-actions">
              <button className="btn small" onClick={() => test(c)} title="send a test">
                <Send size={11} />
              </button>
              <button
                className={`btn small${c.enabled ? " primary" : ""}`}
                onClick={() => toggle(c)}
                title={c.enabled ? "enabled — click to pause" : "paused — click to enable"}
              >
                <Check size={11} />
              </button>
              <button className="btn danger small" onClick={() => remove(c)} title="remove">
                <Trash2 size={11} />
              </button>
            </div>
          </div>
        ))}
      </div>
    </Panel>
  );
}

function messageOf(error: unknown): string {
  if (typeof error === "object" && error && "error" in error) {
    return String((error as { error: unknown }).error);
  }
  return JSON.stringify(error);
}
