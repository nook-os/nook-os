// The per-task action menu: three dots on a card, and the same menu on
// right-click.
//
// Cards used to carry a row of buttons — edit, delete, dispatch, start work,
// submit PR — which meant the most destructive action on the board sat one
// mis-click from the drag handle, and a column of twenty cards rendered eighty
// buttons nobody was looking at. Actions belong behind a deliberate gesture;
// the card's job is to be readable.
//
// MAIN-168: this no longer renders its own positioned/dismissed menu. It builds
// a `ContextMenuItem[]` for the shared app-wide context-menu primitive
// (`contextMenu.tsx`) — the board registers these as a card's region (right-
// click) and opens them from the three-dots button via `openAt`. The three
// hover submenus ("Move to", "Move to epic", "Visibility") map onto the
// primitive's `children`.
import React from "react";
import { api, type TaskItem, type LoopJob } from "@nookos/api";
import { VISIBILITY_META } from "@nookos/ui";
import { askConfirm, notify } from "./dialogs";
import type { ContextMenuItem } from "./contextMenu";
import { createLoopJob, loopAction } from "./loop";

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

export interface TaskMenuContext {
  task: TaskItem;
  columns: MenuColumn[];
  /** The board's epics, for "Move to epic" (MAIN-83 AC-5). Omitted → no entry. */
  epics?: { id: string; key?: string | null; title: string; type?: string }[];
  /** The task's loop jobs, if known (drives the loop item's disabled state).
   *  Read from the query cache at open time; undefined when not yet fetched. */
  jobs?: LoopJob[];
  onOpen: () => void;
  onStartWork: (task: TaskItem) => void;
  refresh: () => void;
}

/** Build the task action menu as items for the shared context-menu primitive. */
export function taskMenuItems({
  task,
  columns,
  epics,
  jobs,
  onOpen,
  onStartWork,
  refresh,
}: TaskMenuContext): ContextMenuItem[] {
  // Every mutating action refreshes the board after it lands; the primitive has
  // already closed the menu by the time onSelect runs.
  const run = async (fn: () => Promise<unknown>) => {
    await fn();
    refresh();
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

  // Change who may see this card (MAIN-103). A 403 from the MAIN-85 gate
  // surfaces via the shared write-failure toast, untouched.
  const currentVisibility = task.visibility ?? "team";
  const setVisibility = (visibility: string) =>
    run(() =>
      api.PATCH("/api/v1/tasks/{id}", {
        params: { path: { id: task.id } },
        body: { visibility },
      }),
    );

  const copy = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      // Clipboard access can be refused (insecure origin, denied permission).
      // Say so rather than pretending it worked.
      await notify("Could not copy", `Copy this manually:\n\n${text}`);
    }
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

  // The ticket's loop entry action (MAIN-128): an epic runs the decomposer, any
  // other ticket drafts a spec. Disabled — with the reason as a hint — while a
  // job is already active on it, exactly as the panel header decides.
  const loop = loopAction(task.type, jobs);

  const items: ContextMenuItem[] = [];

  items.push({ label: "Open", onSelect: onOpen });

  // "Move to" — every column except the one it is already in.
  items.push({
    label: "Move to",
    children: columns
      .filter((c) => c.id !== task.column_id)
      .map((c) => ({ label: c.name, onSelect: () => move(c.id) })),
  });

  // "Move to epic" — only for a non-epic task, and only when epics are known.
  if (task.type !== "epic" && !!epics) {
    const epicChoices = (epics ?? []).filter(
      (e) => e.id !== task.id && e.id !== task.parent_task_id,
    );
    const kids: ContextMenuItem[] = [];
    if (task.parent_task_id) {
      kids.push({ label: "— No epic —", onSelect: () => moveToEpic(null) });
    }
    if (epicChoices.length === 0 && !task.parent_task_id) {
      kids.push({ label: "no epics on this board", disabled: true });
    }
    for (const e of epicChoices) {
      kids.push({
        label: `${e.key ? `${e.key} ` : ""}${e.title}`,
        onSelect: () => moveToEpic(e.id),
      });
    }
    items.push({ label: "Move to epic", children: kids });
  }

  // "Visibility" — each option with its glyph; the current one is checked.
  items.push({
    label: "Visibility",
    children: VISIBILITY_META.map((v) => ({
      label: v.label,
      icon: <v.Icon size={12} />,
      hint: currentVisibility === v.value ? "✓" : undefined,
      onSelect: () => setVisibility(v.value),
    })),
  });

  items.push({ separator: true });
  if (task.key) items.push({ label: "Copy key", onSelect: () => copy(task.key!) });
  if (task.url) items.push({ label: "Copy link", onSelect: () => copy(task.url!) });

  items.push({ separator: true });
  items.push({
    label: loop.label,
    disabled: loop.disabled,
    hint: loop.disabled ? (loop.reason ?? undefined) : undefined,
    onSelect: () => createLoopJob(loop.kind, task.id).then(refresh),
  });

  items.push({ separator: true });
  // Offered by capability, so nothing here can only fail.
  if (!task.branch) {
    items.push({ label: "Start work…", onSelect: () => onStartWork(task) });
  }
  if (task.branch && !task.pr_url) {
    items.push({ label: "Submit PR", onSelect: submitPr });
  }
  if (!task.assigned_node_id) {
    items.push({
      label: "Dispatch to a node",
      onSelect: () =>
        run(() =>
          api.POST("/api/v1/tasks/{id}/dispatch", { params: { path: { id: task.id } } }),
        ),
    });
  }
  if (task.worktree_path) {
    items.push({
      label: "Prune worktree",
      onSelect: () =>
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
    });
  }

  items.push({ separator: true });
  items.push({ label: task.archived_at ? "Unarchive" : "Archive", onSelect: archive });
  items.push({ label: "Delete", danger: true, onSelect: del });

  return items;
}
