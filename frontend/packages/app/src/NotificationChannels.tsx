// Where notifications go besides this browser.
//
// The form is built from `/notification-channels/kinds` rather than from a
// copy of the provider list in here. That is the whole reason the server
// describes its own fields: adding Discord on the backend makes it appear in
// this UI without a frontend change, and a frontend that had its own list
// would be a second place to forget.
import React, { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { BellRing, Check, Filter, Plus, Send, Trash2, TriangleAlert } from "lucide-react";
import {
  api,
  type ChannelKind,
  type NotificationChannel,
  type NotificationKind,
} from "@nookos/api";
import { Panel, Select } from "@nookos/ui";
import { askConfirm, notify } from "./dialogs";
import { decode, encode, groupsOf } from "./notificationKinds";

const LEVELS = ["info", "success", "warning", "error"];

/**
 * The grouped checklist for a channel's notification filter (MAIN-92). It reads
 * the channel's stored `kinds` array and lets a user shape it without typing
 * prefix strings: "Everything" (the empty array) is a first-class pill distinct
 * from all-boxes-checked, a group checkbox ticks its whole group, and a
 * hand-entered filter matching no catalogued kind survives as a removable chip.
 */
function KindsChecklist({
  catalog,
  value,
  onChange,
}: {
  catalog: NotificationKind[];
  value: string[];
  onChange: (kinds: string[]) => void;
}) {
  const { checked, chips } = decode(value, catalog);
  const everything = value.length === 0;
  const groups = groupsOf(catalog);

  const emit = (nextChecked: Set<string>, nextChips: string[]) =>
    onChange(encode({ everything: false, checked: nextChecked, chips: nextChips }, catalog));

  const toggleKind = (id: string) => {
    const next = new Set(checked);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    emit(next, chips);
  };
  const toggleGroup = (ids: string[], allOn: boolean) => {
    const next = new Set(checked);
    for (const id of ids) {
      if (allOn) next.delete(id);
      else next.add(id);
    }
    emit(next, chips);
  };

  return (
    <div className="chan-kinds">
      <div className="chan-kinds-mode">
        <button
          className={`task-chip ${everything ? "on" : ""}`}
          onClick={() => onChange([])}
          title="deliver every notification"
        >
          Everything
        </button>
        <span className="faint small">
          {everything ? "every notification is delivered here" : "only the ticked kinds"}
        </span>
      </div>

      {groups.map((g) => {
        const gids = g.kinds.map((k) => k.id);
        const all = gids.every((id) => checked.has(id));
        const some = gids.some((id) => checked.has(id));
        return (
          <div className="chan-kind-group" key={g.prefix}>
            <label className="chan-kind chan-kind-hdr">
              <input
                type="checkbox"
                checked={all}
                ref={(el) => {
                  if (el) el.indeterminate = some && !all;
                }}
                onChange={() => toggleGroup(gids, all)}
              />
              <span className="bright">{g.label}</span>
            </label>
            {g.kinds.map((k) => (
              <label className="chan-kind" key={k.id} title={k.description}>
                <input
                  type="checkbox"
                  checked={checked.has(k.id)}
                  onChange={() => toggleKind(k.id)}
                />
                <span>{k.label}</span>
              </label>
            ))}
          </div>
        );
      })}

      {chips.length > 0 && (
        <div className="chan-kind-chips">
          <span className="faint small">custom filters</span>
          {chips.map((c) => (
            <button
              key={c}
              className="task-chip on"
              onClick={() => emit(checked, chips.filter((x) => x !== c))}
              title="a hand-entered prefix — click to remove"
            >
              {c} ✕
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

export function NotificationChannels() {
  const qc = useQueryClient();
  const [adding, setAdding] = useState(false);
  const [kind, setKind] = useState("ntfy");
  const [name, setName] = useState("");
  const [config, setConfig] = useState<Record<string, string>>({});
  const [levels, setLevels] = useState<string[]>([]);
  // New channels default to "Everything" ([]) — matching backend semantics (AC-2).
  const [newKinds, setNewKinds] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  // Which existing channel's filter checklist is expanded (by id), or null.
  const [editing, setEditing] = useState<string | null>(null);

  const { data: kinds } = useQuery({
    queryKey: ["channel-kinds"],
    queryFn: async () =>
      (await api.GET("/api/v1/notification-channels/kinds")).data ?? [],
  });
  // The notification-kind catalog (MAIN-91): what the checklist is built from.
  const { data: catalog } = useQuery({
    queryKey: ["notification-kinds"],
    queryFn: async () => (await api.GET("/api/v1/notifications/kinds")).data ?? [],
  });
  const { data: channels } = useQuery({
    queryKey: ["channels"],
    queryFn: async () => (await api.GET("/api/v1/notification-channels")).data ?? [],
  });

  const spec: ChannelKind | undefined = (kinds ?? []).find((k) => k.id === kind);
  const bust = () => qc.invalidateQueries({ queryKey: ["channels"] });

  const setChannelKinds = async (c: NotificationChannel, next: string[]) => {
    await api.PATCH("/api/v1/notification-channels/{id}", {
      params: { path: { id: c.id } },
      body: { kinds: next },
    });
    bust();
  };

  const create = async () => {
    setBusy(true);
    const { error } = await api.POST("/api/v1/notification-channels", {
      body: {
        kind,
        name: name.trim() || spec?.label || kind,
        config,
        levels,
        kinds: newKinds,
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
    setNewKinds([]);
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

            <div className="chan-row chan-row-top">
              <span className="faint small">Deliver</span>
              <KindsChecklist
                catalog={catalog ?? []}
                value={newKinds}
                onChange={setNewKinds}
              />
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
              <button
                className={`btn small${editing === c.id ? " primary" : ""}`}
                onClick={() => setEditing(editing === c.id ? null : c.id)}
                title="choose which notifications reach here"
              >
                <Filter size={11} />
              </button>
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
            {editing === c.id && (
              <div className="chan-item-filter">
                <KindsChecklist
                  catalog={catalog ?? []}
                  value={c.kinds}
                  onChange={(next) => setChannelKinds(c, next)}
                />
              </div>
            )}
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
