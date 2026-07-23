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
  anchor,
  onClose,
  onStartWork,
  onOpen,
  refresh,
}: {
  task: TaskItem;
  columns: MenuColumn[];
  anchor: MenuAnchor;
  onClose: () => void;
  onStartWork: (task: TaskItem) => void;
  onOpen: () => void;
  refresh: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState(anchor);
  const [submenu, setSubmenu] = useState<"move" | null>(null);

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
      {item("Delete", del, { danger: true })}
    </div>
  );
}
