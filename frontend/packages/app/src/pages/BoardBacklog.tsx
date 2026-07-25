// The Backlog tab of the Board page (MAIN-82): a dense, prioritized list of the
// refinement queue — backlog-column tasks and epics — that used to crowd the
// kanban as a Triage column. Fed by the same board_detail fetch as the kanban
// tab; no new endpoint. Rows reuse the card helpers (priority mark, TypeBadge,
// labels, workspace) so a backlog row reads like a compact card.
import React, { useState } from "react";
import { ArrowRight, MoreHorizontal, Plus, Rocket } from "lucide-react";
import { type TaskItem } from "@nookos/api";
import { TypeBadge } from "@nookos/ui";

import { priorityMeta, previewText } from "../taskmeta";

/// A one-line composer that files a new task into the backlog column.
function BacklogComposer({ onAdd }: { onAdd: (title: string) => void }) {
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
        placeholder="Add a backlog task…"
        value={title}
        onChange={(e) => setTitle(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            submit();
          }
        }}
      />
      <button className="btn small" onClick={submit} disabled={!title.trim()} title="add to backlog">
        <Plus size={12} /> add
      </button>
    </div>
  );
}

function BacklogRow({
  task,
  workspaceName,
  selected,
  blocked,
  onOpen,
  onMenu,
  onSendToBoard,
  onDispatch,
}: {
  task: TaskItem;
  workspaceName?: string;
  selected: boolean;
  blocked: boolean;
  onOpen: () => void;
  onMenu: (anchor: { x: number; y: number }) => void;
  onSendToBoard?: () => void;
  onDispatch?: () => void;
}) {
  const isEpic = task.type === "epic";
  const prio = priorityMeta(task.priority ?? 0);
  return (
    <div
      className={`backlog-row${selected ? " selected" : ""}${blocked ? " blocked" : ""}`}
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
      <span className="backlog-row-actions" onClick={(e) => e.stopPropagation()}>
        {/* Epics are containers — not sent to the board or dispatched (NG-1/flat). */}
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
    </div>
  );
}

export function BoardBacklog({
  tasks,
  wsName,
  selectedId,
  blockedIds,
  canSendToBoard,
  onAdd,
  onOpen,
  onMenu,
  onSendToBoard,
  onDispatch,
}: {
  tasks: TaskItem[];
  wsName: Map<string, string>;
  selectedId: string | null;
  blockedIds: Set<string>;
  canSendToBoard: boolean;
  onAdd: (title: string) => void;
  onOpen: (key: string) => void;
  onMenu: (task: TaskItem, anchor: { x: number; y: number }) => void;
  onSendToBoard: (taskId: string) => void;
  onDispatch: (taskId: string) => void;
}) {
  return (
    <div className="backlog">
      <BacklogComposer onAdd={onAdd} />
      {tasks.length === 0 ? (
        <div className="board-no-matches faint small">The backlog is empty.</div>
      ) : (
        <div className="backlog-list">
          {tasks.map((t) => (
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
        </div>
      )}
    </div>
  );
}
