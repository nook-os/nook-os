// The per-task action menu: three dots on a card, and the same menu on
// right-click.
//
// Cards used to carry a row of buttons — edit, delete, dispatch, start work,
// submit PR — which meant the most destructive action on the board sat one
// mis-click from the drag handle, and a column of twenty cards rendered eighty
// buttons nobody was looking at. Actions belong behind a deliberate gesture;
// the card's job is to be readable.
import React, { useEffect, useLayoutEffect, useRef, useState } from "react";
import { api, type TaskItem } from "@nookos/api";
import { VISIBILITY_META } from "@nookos/ui";
import { askConfirm, notify } from "./dialogs";

export interface MenuColumn {
  id: string;
  name: string;
  type?: string;
}

/** Where a menu was opened, in viewport coordinates. */
export interface MenuAnchor {
  x: number;
  y: number;
}

export function TaskMenu({
  task,
  columns,
  epics,
  anchor,
  onClose,
  onStartWork,
  onOpen,
  refresh,
}: {
  task: TaskItem;
  columns: MenuColumn[];
  /** The board's epics, for "Move to epic" (MAIN-83 AC-5). Omitted → no entry. */
  epics?: { id: string; key?: string | null; title: string; type?: string }[];
  anchor: MenuAnchor;
  onClose: () => void;
  onStartWork: (task: TaskItem) => void;
  onOpen: () => void;
  refresh: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState(anchor);
  const [submenu, setSubmenu] = useState<"move" | "epic" | "visibility" | null>(null);

  // Keep the menu on screen. Opened from a card near the right edge or the
  // bottom of a tall column, a naively-placed menu renders half off-screen and
  // the items you wanted are the ones you cannot reach.
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    setPos({
      x: Math.min(anchor.x, window.innerWidth - r.width - 8),
      y: Math.min(anchor.y, window.innerHeight - r.height - 8),
    });
  }, [anchor]);

  useEffect(() => {
    const away = () => onClose();
    const esc = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    // `mousedown`, not `click`: a click listener added during a click event
    // fires on that very event and the menu closes as it opens.
    window.addEventListener("mousedown", away);
    window.addEventListener("keydown", esc);
    return () => {
      window.removeEventListener("mousedown", away);
      window.removeEventListener("keydown", esc);
    };
  }, [onClose]);

  const run = async (fn: () => Promise<unknown>) => {
    await fn();
    refresh();
    onClose();
  };

  const move = (column_id: string) =>
    run(() =>
      api.PATCH("/api/v1/tasks/{id}", {
        params: { path: { id: task.id } },
        body: { column_id },
      }),
    );

  // File under an epic, or detach with `null` (MAIN-83 AC-5). A backend refusal
  // (non-epic parent) surfaces via the write-failure toast, untouched (AC-6).
  const moveToEpic = (parent: string | null) =>
    run(() =>
      api.PATCH("/api/v1/tasks/{id}", {
        params: { path: { id: task.id } },
        body: { parent },
      }),
    );
  // Only for a non-epic task, and only when epics are known. Exclude self.
  const epicChoices = (epics ?? []).filter((e) => e.id !== task.id);
  const canMoveToEpic = task.type !== "epic" && !!epics;

  // Change who may see this card (MAIN-103). A 403 from the MAIN-85 gate ("this
  // needs tenant owner or admin") surfaces via the shared write-failure toast,
  // untouched — the same way every other menu PATCH reports a refusal.
  const currentVisibility = task.visibility ?? "team";
  const setVisibility = (visibility: string) =>
    run(() =>
      api.PATCH("/api/v1/tasks/{id}", {
        params: { path: { id: task.id } },
        body: { visibility },
      }),
    );

  const copy = async (text: string, what: string) => {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      // Clipboard access can be refused (insecure origin, denied permission).
      // Say so rather than pretending it worked.
      await notify("Could not copy", `Copy this manually:\n\n${text}`);
    }
    onClose();
    void what;
  };

  const del = () =>
    run(async () => {
      const ok = await askConfirm({
        title: "Delete task",
        description: `"${task.title}" will be removed from the board.`,
        confirmLabel: "delete",
        danger: true,
      });
      if (ok) {
        await api.DELETE("/api/v1/tasks/{id}", { params: { path: { id: task.id } } });
      }
    });

  const archive = () =>
    run(() =>
      api.POST(
        task.archived_at
          ? "/api/v1/tasks/{id}/unarchive"
          : "/api/v1/tasks/{id}/archive",
        { params: { path: { id: task.id } } },
      ),
    );

  const submitPr = () =>
    run(async () => {
      const { error } = await api.POST("/api/v1/tasks/{id}/submit-pr", {
        params: { path: { id: task.id } },
        body: { pr_url: null },
      });
      if (error) {
        await notify(
          "Could not submit",
          typeof error === "object" && error && "error" in error
            ? String((error as { error: unknown }).error)
            : JSON.stringify(error),
        );
      }
    });

  const item = (
    label: string,
    onClick: () => void,
    opts: { danger?: boolean; sub?: boolean } = {},
  ) => (
    <button
      key={label}
      className={`ctx-item${opts.danger ? " danger" : ""}`}
      onMouseDown={(e) => e.stopPropagation()}
      onClick={onClick}
      onMouseEnter={() => !opts.sub && setSubmenu(null)}
    >
      {label}
      {opts.sub && <span className="ctx-arrow">›</span>}
    </button>
  );

  return (
    <div
      ref={ref}
      className="ctx-menu"
      style={{ left: pos.x, top: pos.y }}
      onMouseDown={(e) => e.stopPropagation()}
      onContextMenu={(e) => e.preventDefault()}
    >
      {item("Open", () => {
        onOpen();
        onClose();
      })}

      <div
        className="ctx-sub-host"
        onMouseEnter={() => setSubmenu("move")}
        onMouseLeave={() => setSubmenu(null)}
      >
        {item("Move to", () => setSubmenu(submenu === "move" ? null : "move"), { sub: true })}
        {submenu === "move" && (
          <div className="ctx-submenu">
            {columns.map((c) =>
              c.id === task.column_id ? null : (
                <button
                  key={c.id}
                  className="ctx-item"
                  onMouseDown={(e) => e.stopPropagation()}
                  onClick={() => move(c.id)}
                >
                  {c.name}
                </button>
              ),
            )}
          </div>
        )}
      </div>

      {canMoveToEpic && (
        <div
          className="ctx-sub-host"
          onMouseEnter={() => setSubmenu("epic")}
          onMouseLeave={() => setSubmenu(null)}
        >
          {item("Move to epic", () => setSubmenu(submenu === "epic" ? null : "epic"), {
            sub: true,
          })}
          {submenu === "epic" && (
            <div className="ctx-submenu">
              {task.parent_task_id && (
                <button
                  className="ctx-item"
                  onMouseDown={(e) => e.stopPropagation()}
                  onClick={() => moveToEpic(null)}
                >
                  — No epic —
                </button>
              )}
              {epicChoices.length === 0 && !task.parent_task_id && (
                <span className="ctx-item faint">no epics on this board</span>
              )}
              {epicChoices.map((e) =>
                e.id === task.parent_task_id ? null : (
                  <button
                    key={e.id}
                    className="ctx-item"
                    onMouseDown={(ev) => ev.stopPropagation()}
                    onClick={() => moveToEpic(e.id)}
                  >
                    {e.key ? `${e.key} ` : ""}
                    {e.title}
                  </button>
                ),
              )}
            </div>
          )}
        </div>
      )}

      <div
        className="ctx-sub-host"
        onMouseEnter={() => setSubmenu("visibility")}
        onMouseLeave={() => setSubmenu(null)}
      >
        {item(
          "Visibility",
          () => setSubmenu(submenu === "visibility" ? null : "visibility"),
          { sub: true },
        )}
        {submenu === "visibility" && (
          <div className="ctx-submenu">
            {VISIBILITY_META.map((v) => (
              <button
                key={v.value}
                className="ctx-item"
                onMouseDown={(e) => e.stopPropagation()}
                onClick={() => setVisibility(v.value)}
                title={v.tooltip}
              >
                <v.Icon size={12} style={{ marginRight: 6, verticalAlign: "-1px" }} />
                {v.label}
                {currentVisibility === v.value && <span className="ok"> ✓</span>}
              </button>
            ))}
          </div>
        )}
      </div>

      <div className="ctx-sep" />

      {task.key && item("Copy key", () => copy(task.key!, "key"))}
      {task.url && item("Copy link", () => copy(task.url!, "link"))}

      <div className="ctx-sep" />

      {/* Offered by capability, so nothing here can only fail. */}
      {!task.branch && item("Start work…", () => {
        onStartWork(task);
        onClose();
      })}
      {task.branch && !task.pr_url && item("Submit PR", submitPr)}
      {!task.assigned_node_id &&
        item("Dispatch to a node", () =>
          run(() =>
            api.POST("/api/v1/tasks/{id}/dispatch", { params: { path: { id: task.id } } }),
          ),
        )}
      {task.worktree_path &&
        item("Prune worktree", () =>
          run(async () => {
            const ok = await askConfirm({
              title: "Prune worktree",
              description: `Remove this task's checkout from ${task.worktree_path}?`,
              confirmLabel: "prune",
              danger: true,
            });
            if (ok) {
              await api.POST("/api/v1/tasks/{id}/prune-worktree", {
                params: { path: { id: task.id } },
              });
            }
          }),
        )}

      <div className="ctx-sep" />
      {item(task.archived_at ? "Unarchive" : "Archive", archive)}
      {item("Delete", del, { danger: true })}
    </div>
  );
}
