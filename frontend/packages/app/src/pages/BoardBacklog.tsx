// The Backlog tab of the Board page (MAIN-82/83): a dense, prioritized list of
// the refinement queue — grouped by epic (MAIN-83). One collapsible section per
// epic, each with a done/total progress count and all its children; a final "No
// epic" section holds parentless backlog tasks. Fed by the same board_detail
// fetch as the kanban tab; no new endpoint. Rows reuse the card helpers.
import React, { useState } from "react";
import { ArrowRight, ChevronDown, ChevronRight, MoreHorizontal, Plus, Rocket } from "lucide-react";
import { type TaskItem } from "@nookos/api";
import { TypeBadge } from "@nookos/ui";

import { priorityMeta, previewText } from "../taskmeta";
import type { BacklogGroups, EpicSection } from "./Board";

const COLLAPSE_KEY = "nook.backlog.collapsed";

/// Collapse state that survives a reload (MAIN-83 AC-1), keyed by epic id in
/// localStorage. Returns the set of collapsed ids and a toggle.
function useCollapsed(): [Set<string>, (id: string) => void] {
  const [ids, setIds] = useState<Set<string>>(() => {
    try {
      return new Set(JSON.parse(localStorage.getItem(COLLAPSE_KEY) ?? "[]"));
    } catch {
      return new Set();
    }
  });
  const toggle = (id: string) =>
    setIds((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      try {
        localStorage.setItem(COLLAPSE_KEY, JSON.stringify([...next]));
      } catch {
        /* a full/blocked storage must not break the board */
      }
      return next;
    });
  return [ids, toggle];
}

/// A backlog task's status for a chip: `archived`, or its column type.
function statusOf(task: TaskItem, colTypeById: Map<string, string | undefined>): string {
  if (task.archived_at) return "archived";
  return colTypeById.get(task.column_id) ?? "";
}

/// A one-line composer that files into the backlog.
function BacklogComposer({
  onAdd,
  placeholder,
  label,
}: {
  onAdd: (title: string) => void;
  placeholder: string;
  label: string;
}) {
  const [title, setTitle] = useState("");
  const submit = () => {
    const t = title.trim();
    if (!t) return;
    onAdd(t);
    setTitle("");
  };
  return (
    <div className="backlog-composer">
      <input
        className="composer-input"
        placeholder={placeholder}
        value={title}
        onChange={(e) => setTitle(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            submit();
          }
        }}
      />
      <button className="btn small" onClick={submit} disabled={!title.trim()} title={placeholder}>
        <Plus size={12} /> {label}
      </button>
    </div>
  );
}

function BacklogRow({
  task,
  workspaceName,
  status,
  selected,
  blocked,
  readOnly,
  onOpen,
  onMenu,
  onSendToBoard,
  onDispatch,
}: {
  task: TaskItem;
  workspaceName?: string;
  /** Status chip (column type or `archived`); omitted for the flat "No epic" rows. */
  status?: string;
  selected: boolean;
  blocked: boolean;
  /** An on-board child: opens the detail, no inline actions (MAIN-83 AC-2). */
  readOnly?: boolean;
  onOpen: () => void;
  onMenu: (anchor: { x: number; y: number }) => void;
  onSendToBoard?: () => void;
  onDispatch?: () => void;
}) {
  const isEpic = task.type === "epic";
  const prio = priorityMeta(task.priority ?? 0);
  return (
    <div
      className={`backlog-row${selected ? " selected" : ""}${blocked ? " blocked" : ""}${task.archived_at ? " archived" : ""}`}
      onClick={onOpen}
    >
      {!!task.priority && (
        <span className="card-prio" style={{ color: prio.color }} title={`priority: ${prio.label}`}>
          {prio.mark}
        </span>
      )}
      {task.type && task.type !== "task" && <TypeBadge type={task.type} compact />}
      <span className="card-key mono">{task.key ?? ""}</span>
      <span className="backlog-row-title">{task.title}</span>
      {status && <span className="backlog-status">{status}</span>}
      {(task.labels ?? []).map((l) => (
        <span key={l.id} className="card-label" style={{ borderColor: l.color, color: l.color }}>
          {l.name}
        </span>
      ))}
      {workspaceName && (
        <span className="card-workspace" title={`workspace: ${workspaceName}`}>
          {workspaceName}
        </span>
      )}
      {previewText(task.description) && (
        <span className="backlog-row-preview faint">{previewText(task.description)}</span>
      )}
      {!readOnly && (
        <span className="backlog-row-actions" onClick={(e) => e.stopPropagation()}>
          {/* Epics are containers — not sent to the board or dispatched. */}
          {!isEpic && onSendToBoard && (
            <button className="btn small" onClick={onSendToBoard} title="send to board (Todo)">
              <ArrowRight size={11} /> board
            </button>
          )}
          {!isEpic && onDispatch && (
            <button className="btn small" onClick={onDispatch} title="dispatch to a node">
              <Rocket size={11} />
            </button>
          )}
          <button
            className="card-menu-btn"
            title="task actions"
            onClick={(e) => {
              e.stopPropagation();
              const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
              onMenu({ x: r.right, y: r.bottom });
            }}
          >
            <MoreHorizontal size={13} />
          </button>
        </span>
      )}
    </div>
  );
}

function EpicGroup({
  section,
  colTypeById,
  wsName,
  selectedId,
  blockedIds,
  canSendToBoard,
  collapsed,
  onToggle,
  onOpen,
  onMenu,
  onSendToBoard,
  onDispatch,
  onAddChild,
}: {
  section: EpicSection;
  colTypeById: Map<string, string | undefined>;
  wsName: Map<string, string>;
  selectedId: string | null;
  blockedIds: Set<string>;
  canSendToBoard: boolean;
  collapsed: boolean;
  onToggle: () => void;
  onOpen: (key: string) => void;
  onMenu: (task: TaskItem, anchor: { x: number; y: number }) => void;
  onSendToBoard: (taskId: string) => void;
  onDispatch: (taskId: string) => void;
  onAddChild: (epicId: string, title: string) => void;
}) {
  const { epic, children, done, total } = section;
  return (
    <div className="backlog-epic">
      {/* Clicking the head opens the epic's detail like any other row; the
         chevron is its own button so collapsing the group stays separate from
         opening it (MAIN-83 — the head used to only toggle, so an epic could
         never be opened). */}
      <div
        className="backlog-epic-head"
        onClick={() => onOpen(epic.key ?? epic.id)}
      >
        <button
          className="card-menu-btn"
          title={collapsed ? "expand" : "collapse"}
          onClick={(e) => {
            e.stopPropagation();
            onToggle();
          }}
        >
          {collapsed ? <ChevronRight size={13} /> : <ChevronDown size={13} />}
        </button>
        <TypeBadge type="epic" compact />
        <span className="card-key mono">{epic.key ?? ""}</span>
        <span className="backlog-epic-title">{epic.title}</span>
        {(epic.labels ?? []).map((l) => (
          <span
            key={l.id}
            className="card-label"
            style={{ borderColor: l.color, color: l.color }}
          >
            {l.name}
          </span>
        ))}
        <span className="backlog-epic-progress faint" title="done / total children">
          {done}/{total}
        </span>
        <button
          className="card-menu-btn"
          title="epic actions"
          onClick={(e) => {
            e.stopPropagation();
            const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
            onMenu(epic, { x: r.right, y: r.bottom });
          }}
        >
          <MoreHorizontal size={13} />
        </button>
      </div>
      {!collapsed && (
        <div className="backlog-epic-body">
          {children.length === 0 ? (
            <div className="backlog-epic-empty faint small">No tickets under this epic yet.</div>
          ) : (
            children.map((t) => {
              const onBoard = colTypeById.get(t.column_id) !== "backlog";
              return (
                <BacklogRow
                  key={t.id}
                  task={t}
                  workspaceName={t.workspace_id ? wsName.get(t.workspace_id) : undefined}
                  status={statusOf(t, colTypeById)}
                  selected={selectedId === t.key || selectedId === t.id}
                  blocked={blockedIds.has(t.id)}
                  readOnly={onBoard}
                  onOpen={() => onOpen(t.key ?? t.id)}
                  onMenu={(anchor) => onMenu(t, anchor)}
                  onSendToBoard={canSendToBoard ? () => onSendToBoard(t.id) : undefined}
                  onDispatch={() => onDispatch(t.id)}
                />
              );
            })
          )}
          <BacklogComposer
            onAdd={(title) => onAddChild(epic.id, title)}
            placeholder="Add a ticket under this epic…"
            label="ticket"
          />
        </div>
      )}
    </div>
  );
}

export function BoardBacklog({
  groups,
  colTypeById,
  wsName,
  selectedId,
  blockedIds,
  canSendToBoard,
  onAddEpic,
  onAddChild,
  onAddBacklog,
  onOpen,
  onMenu,
  onSendToBoard,
  onDispatch,
}: {
  groups: BacklogGroups;
  colTypeById: Map<string, string | undefined>;
  wsName: Map<string, string>;
  selectedId: string | null;
  blockedIds: Set<string>;
  canSendToBoard: boolean;
  onAddEpic: (title: string) => void;
  onAddChild: (epicId: string, title: string) => void;
  onAddBacklog: (title: string) => void;
  onOpen: (key: string) => void;
  onMenu: (task: TaskItem, anchor: { x: number; y: number }) => void;
  onSendToBoard: (taskId: string) => void;
  onDispatch: (taskId: string) => void;
}) {
  const [collapsed, toggle] = useCollapsed();
  const empty = groups.epics.length === 0 && groups.noEpic.length === 0;
  return (
    <div className="backlog">
      <BacklogComposer onAdd={onAddEpic} placeholder="New epic…" label="epic" />
      {empty && <div className="board-no-matches faint small">The backlog is empty.</div>}

      {groups.epics.map((section) => (
        <EpicGroup
          key={section.epic.id}
          section={section}
          colTypeById={colTypeById}
          wsName={wsName}
          selectedId={selectedId}
          blockedIds={blockedIds}
          canSendToBoard={canSendToBoard}
          collapsed={collapsed.has(section.epic.id)}
          onToggle={() => toggle(section.epic.id)}
          onOpen={onOpen}
          onMenu={onMenu}
          onSendToBoard={onSendToBoard}
          onDispatch={onDispatch}
          onAddChild={onAddChild}
        />
      ))}

      {/* Parentless backlog tasks — the "No epic" section (MAIN-83 AC-1). */}
      <div className="backlog-epic">
        <div className="backlog-epic-head no-epic">
          <span className="backlog-epic-title faint">No epic</span>
          <span className="backlog-epic-progress faint">{groups.noEpic.length}</span>
        </div>
        <div className="backlog-epic-body">
          {groups.noEpic.map((t) => (
            <BacklogRow
              key={t.id}
              task={t}
              workspaceName={t.workspace_id ? wsName.get(t.workspace_id) : undefined}
              selected={selectedId === t.key || selectedId === t.id}
              blocked={blockedIds.has(t.id)}
              onOpen={() => onOpen(t.key ?? t.id)}
              onMenu={(anchor) => onMenu(t, anchor)}
              onSendToBoard={canSendToBoard ? () => onSendToBoard(t.id) : undefined}
              onDispatch={() => onDispatch(t.id)}
            />
          ))}
          <BacklogComposer onAdd={onAddBacklog} placeholder="Add a backlog task…" label="task" />
        </div>
      </div>
    </div>
  );
}
