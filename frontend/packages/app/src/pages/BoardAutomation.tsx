// Board automation editor (MAIN-73). A modal that lists rules grouped by column
// TYPE and lets you add/remove actions with their config fields, then PATCHes
// the board. The pure state helpers below are exported so the reducer logic is
// unit-testable without mounting the modal.
import React, { useState } from "react";
import { Plus, Trash2, X } from "lucide-react";
import { api } from "@nookos/api";

export type ActionKind = "add_board_label" | "remove_board_label" | "notify";

export interface AutomationAction {
  kind: ActionKind;
  label?: string;
  title?: string;
  body?: string;
}

/** A map from column type to its ordered action list. */
export type Automation = Record<string, AutomationAction[]>;

/** The column types a rule may target, in board order, with a human label. */
export const COLUMN_TYPES: { type: string; label: string }[] = [
  { type: "backlog", label: "Triage" },
  { type: "unstarted", label: "Todo" },
  { type: "started", label: "In Progress" },
  { type: "review", label: "In Review" },
  { type: "completed", label: "Done" },
  { type: "canceled", label: "Canceled" },
];

export const ACTION_LABELS: Record<ActionKind, string> = {
  add_board_label: "Add label",
  remove_board_label: "Remove label",
  notify: "Notify",
};

/** A fresh action of a kind, with its config fields blank. */
export function defaultAction(kind: ActionKind): AutomationAction {
  return kind === "notify" ? { kind, title: "", body: "" } : { kind, label: "" };
}

/** Append an action to a column type's list. */
export function addAction(auto: Automation, type: string, kind: ActionKind): Automation {
  const list = auto[type] ?? [];
  return { ...auto, [type]: [...list, defaultAction(kind)] };
}

/** Remove the action at `idx` from a column type's list. */
export function removeAction(auto: Automation, type: string, idx: number): Automation {
  const list = (auto[type] ?? []).filter((_, i) => i !== idx);
  return { ...auto, [type]: list };
}

/** Patch one field of one action. Switching `kind` resets to that kind's shape. */
export function updateAction(
  auto: Automation,
  type: string,
  idx: number,
  patch: Partial<AutomationAction>,
): Automation {
  const list = (auto[type] ?? []).map((a, i) => {
    if (i !== idx) return a;
    if (patch.kind && patch.kind !== a.kind) return defaultAction(patch.kind);
    return { ...a, ...patch };
  });
  return { ...auto, [type]: list };
}

/**
 * Prepare the config for storage: drop empty column-type lists and trim/omit
 * blank optional fields, so the server stores exactly what the user meant. A
 * blank label is left in so the server's validation rejects it visibly rather
 * than silently dropping the action.
 */
export function cleanAutomation(auto: Automation): Automation {
  const out: Automation = {};
  for (const { type } of COLUMN_TYPES) {
    const list = (auto[type] ?? []).map((a) => {
      if (a.kind === "notify") {
        const n: AutomationAction = { kind: "notify" };
        if (a.title?.trim()) n.title = a.title.trim();
        if (a.body?.trim()) n.body = a.body.trim();
        return n;
      }
      return { kind: a.kind, label: (a.label ?? "").trim() };
    });
    if (list.length) out[type] = list;
  }
  return out;
}

function coerce(value: unknown): Automation {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Automation)
    : {};
}

export function AutomationDialog({
  boardId,
  boardName,
  automation,
  onClose,
  onSaved,
}: {
  boardId: string;
  boardName: string;
  automation: unknown;
  onClose: () => void;
  onSaved: () => void;
}) {
  const [auto, setAuto] = useState<Automation>(() => coerce(automation));
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const save = async () => {
    setSaving(true);
    setError(null);
    const { error: err } = await api.PATCH("/api/v1/boards/{id}", {
      params: { path: { id: boardId } },
      body: { name: boardName, automation: cleanAutomation(auto) },
    });
    setSaving(false);
    if (err) {
      // The server validates on write; surface its message rather than closing.
      setError(String((err as { message?: string })?.message ?? "could not save"));
      return;
    }
    onSaved();
    onClose();
  };

  return (
    <div className="modal-backdrop" onMouseDown={onClose}>
      <div
        className="modal"
        style={{ width: "min(680px, 94vw)", maxHeight: "88vh", display: "flex", flexDirection: "column" }}
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key === "Escape") onClose();
        }}
      >
        <div className="modal-header">
          Automation · {boardName}
          <button className="btn small" onClick={onClose} title="close" style={{ float: "right" }}>
            <X size={12} />
          </button>
        </div>
        <div className="modal-body" style={{ overflowY: "auto" }}>
          <p className="muted small">
            Rules fire when a task enters a column of the given type — from a drag,
            a move, dispatch, start-work, submit-PR, or a claim. Actions run in
            order and are best-effort; a failure notifies and comments on the card
            without blocking the move.
          </p>
          {COLUMN_TYPES.map(({ type, label }) => {
            const list = auto[type] ?? [];
            return (
              <div key={type} className="field" style={{ borderTop: "1px solid var(--border)", paddingTop: 8 }}>
                <label style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}>
                  <span className="bright">{label}</span>
                  <span style={{ display: "inline-flex", gap: 4 }}>
                    {(Object.keys(ACTION_LABELS) as ActionKind[]).map((k) => (
                      <button
                        key={k}
                        className="btn small"
                        onClick={() => setAuto((a) => addAction(a, type, k))}
                        title={`add ${ACTION_LABELS[k]}`}
                      >
                        <Plus size={10} /> {ACTION_LABELS[k]}
                      </button>
                    ))}
                  </span>
                </label>
                {list.length === 0 ? (
                  <div className="muted small" style={{ padding: "4px 0" }}>
                    No rules — entering this column does nothing.
                  </div>
                ) : (
                  list.map((action, idx) => (
                    <div
                      key={idx}
                      style={{ display: "flex", gap: 6, alignItems: "center", marginBottom: 4 }}
                    >
                      <select
                        className="input"
                        style={{ maxWidth: 150 }}
                        value={action.kind}
                        onChange={(e) =>
                          setAuto((a) =>
                            updateAction(a, type, idx, { kind: e.target.value as ActionKind }),
                          )
                        }
                      >
                        {(Object.keys(ACTION_LABELS) as ActionKind[]).map((k) => (
                          <option key={k} value={k}>
                            {ACTION_LABELS[k]}
                          </option>
                        ))}
                      </select>
                      {action.kind === "notify" ? (
                        <>
                          <input
                            className="input"
                            placeholder="title (tokens: {key} {title} {url})"
                            value={action.title ?? ""}
                            onChange={(e) =>
                              setAuto((a) => updateAction(a, type, idx, { title: e.target.value }))
                            }
                          />
                          <input
                            className="input"
                            placeholder="body (optional)"
                            value={action.body ?? ""}
                            onChange={(e) =>
                              setAuto((a) => updateAction(a, type, idx, { body: e.target.value }))
                            }
                          />
                        </>
                      ) : (
                        <input
                          className="input"
                          placeholder="label"
                          value={action.label ?? ""}
                          onChange={(e) =>
                            setAuto((a) => updateAction(a, type, idx, { label: e.target.value }))
                          }
                        />
                      )}
                      <button
                        className="btn danger small"
                        onClick={() => setAuto((a) => removeAction(a, type, idx))}
                        title="remove action"
                      >
                        <Trash2 size={11} />
                      </button>
                    </div>
                  ))
                )}
              </div>
            );
          })}
          {error && (
            <p className="small" style={{ color: "var(--danger, #d66)" }}>
              {error}
            </p>
          )}
        </div>
        <div className="modal-footer">
          <button className="btn primary" onClick={save} disabled={saving}>
            {saving ? "saving…" : "save"}
          </button>
          <button className="btn" onClick={onClose}>
            cancel
          </button>
        </div>
      </div>
    </div>
  );
}
