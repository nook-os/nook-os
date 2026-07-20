import React, { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import {
  DndContext,
  DragEndEvent,
  PointerSensor,
  useDraggable,
  useDroppable,
  useSensor,
  useSensors,
} from "@dnd-kit/core";
import {
  GitBranch,
  Pencil,
  Play,
  Plus,
  Rocket,
  SquareTerminal,
  Trash2,
  Upload,
  X,
} from "lucide-react";
import { api, type TaskItem } from "@nookos/api";
import { Empty, Panel, Pill } from "@nookos/ui";
import { useNewWork } from "../newwork";

function CardActions({
  task,
  column,
  onStartWork,
  refresh,
}: {
  task: TaskItem;
  column: string;
  onStartWork: (task: TaskItem) => void;
  refresh: () => void;
}) {
  const col = column.toLowerCase();
  const dispatch = async () => {
    const { error } = await api.POST("/api/v1/tasks/{id}/dispatch", {
      params: { path: { id: task.id } },
    });
    if (error) alert(JSON.stringify(error));
    refresh();
  };
  const submitPr = async () => {
    const pr_url = prompt("PR URL (leave blank to auto-generate a compare link):") ?? "";
    const { error } = await api.POST("/api/v1/tasks/{id}/submit-pr", {
      params: { path: { id: task.id } },
      body: { pr_url: pr_url || null },
    });
    if (error) alert(JSON.stringify(error));
    refresh();
  };
  const prune = async () => {
    if (!confirm("Remove this task's worktree checkout?")) return;
    const { error } = await api.POST("/api/v1/tasks/{id}/prune-worktree", {
      params: { path: { id: task.id } },
    });
    if (error) alert(JSON.stringify(error));
    refresh();
  };
  const edit = async () => {
    const title = prompt("Task title", task.title);
    if (title === null) return;
    const description = prompt("Description (optional)", task.description ?? "");
    await api.PATCH("/api/v1/tasks/{id}", {
      params: { path: { id: task.id } },
      body: { title: title.trim() || task.title, description: description ?? "" },
    });
    refresh();
  };
  const del = async () => {
    if (!confirm(`Delete task "${task.title}"?`)) return;
    await api.DELETE("/api/v1/tasks/{id}", { params: { path: { id: task.id } } });
    refresh();
  };

  return (
    <div style={{ display: "flex", gap: 4, marginTop: 6, flexWrap: "wrap", alignItems: "center" }}>
      {col.includes("triage") && (
        <button className="btn small" onClick={dispatch} title="scheduler picks a node">
          <Rocket size={12} /> dispatch
        </button>
      )}
      {(col.includes("todo") || col.includes("triage")) && !task.worktree_path && (
        <button className="btn primary small" onClick={() => onStartWork(task)}>
          <Play size={12} /> start work
        </button>
      )}
      {col.includes("progress") && (
        <button className="btn small" onClick={submitPr}>
          <Upload size={12} /> submit PR
        </button>
      )}
      {col.includes("done") && task.worktree_path && (
        <button className="btn danger small" onClick={prune}>
          <Trash2 size={12} /> prune worktree
        </button>
      )}
      <span style={{ flex: 1 }} />
      <button className="btn small" onClick={edit} title="edit task">
        <Pencil size={11} />
      </button>
      <button className="btn small" onClick={del} title="delete task">
        <X size={11} />
      </button>
    </div>
  );
}

function Card({
  task,
  column,
  onStartWork,
  refresh,
}: {
  task: TaskItem;
  column: string;
  onStartWork: (task: TaskItem) => void;
  refresh: () => void;
}) {
  const { attributes, listeners, setNodeRef, transform, isDragging } =
    useDraggable({ id: task.id });
  return (
    <div
      ref={setNodeRef}
      className="board-card"
      style={{
        transform: transform
          ? `translate(${transform.x}px, ${transform.y}px)`
          : undefined,
        opacity: isDragging ? 0.6 : 1,
        zIndex: isDragging ? 10 : undefined,
        position: isDragging ? "relative" : undefined,
      }}
    >
      <div className="bright" {...attributes} {...listeners} style={{ cursor: "grab" }}>
        {task.title}
      </div>
      {task.description && <div className="desc">{task.description}</div>}
      {(task.branch || task.session_id || task.pr_url) && (
        <div style={{ display: "flex", gap: 6, marginTop: 5, flexWrap: "wrap", alignItems: "center" }}>
          {task.branch && (
            <Pill tone="info">
              <GitBranch size={10} style={{ verticalAlign: "-1px" }} /> {task.branch}
            </Pill>
          )}
          {task.session_id && (
            <Link className="bright small mono" to={`/sessions/${task.session_id}`}>
              <SquareTerminal size={11} style={{ verticalAlign: "-2px" }} /> session
            </Link>
          )}
          {task.pr_url && (
            <a className="small" href={task.pr_url} target="_blank" rel="noreferrer">
              PR ↗
            </a>
          )}
        </div>
      )}
      <CardActions task={task} column={column} onStartWork={onStartWork} refresh={refresh} />
    </div>
  );
}

function Column({
  id,
  name,
  tasks,
  onAdd,
  onStartWork,
  onRename,
  onDelete,
  refresh,
}: {
  id: string;
  name: string;
  tasks: TaskItem[];
  onAdd?: (title: string) => void;
  onStartWork: (task: TaskItem) => void;
  onRename: (name: string) => void;
  onDelete: () => void;
  refresh: () => void;
}) {
  const { setNodeRef, isOver } = useDroppable({ id });
  return (
    <div className="board-column">
      <div className="nook-panel-title">
        <span>
          {name} <span className="faint">({tasks.length})</span>
        </span>
        <span style={{ display: "inline-flex", gap: 3 }}>
          {onAdd && (
            <button
              className="btn small"
              title="add task"
              onClick={() => {
                const title = prompt("Task title");
                if (title?.trim()) onAdd(title.trim());
              }}
            >
              <Plus size={12} />
            </button>
          )}
          <button
            className="btn small"
            title="rename column"
            onClick={() => {
              const n = prompt("Column name", name);
              if (n?.trim()) onRename(n.trim());
            }}
          >
            <Pencil size={11} />
          </button>
          <button
            className="btn small"
            title="delete column (and its tasks)"
            onClick={() => {
              if (confirm(`Delete column "${name}" and its ${tasks.length} task(s)?`)) onDelete();
            }}
          >
            <X size={11} />
          </button>
        </span>
      </div>
      <div
        ref={setNodeRef}
        className="board-cards"
        style={isOver ? { background: "var(--nook-bg-raised)" } : undefined}
      >
        {tasks.map((t) => (
          <Card key={t.id} task={t} column={name} onStartWork={onStartWork} refresh={refresh} />
        ))}
      </div>
    </div>
  );
}

export function BoardPage() {
  const queryClient = useQueryClient();
  const showNewWork = useNewWork((s) => s.show);
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 4 } }),
  );

  const { data: boards } = useQuery({
    queryKey: ["boards"],
    queryFn: async () => (await api.GET("/api/v1/boards")).data ?? [],
  });
  const board = (boards ?? [])[0];

  const { data: detail } = useQuery({
    queryKey: ["boards", board?.id],
    queryFn: async () =>
      (await api.GET("/api/v1/boards/{id}", { params: { path: { id: board!.id } } }))
        .data,
    enabled: !!board,
    refetchInterval: 5000,
  });

  const refresh = () => queryClient.invalidateQueries({ queryKey: ["boards"] });

  if (!board) {
    return (
      <div className="nook-grid" style={{ gridTemplateColumns: "1fr" }}>
        <Panel title="Board">
          <Empty>
            No boards yet.{" "}
            <button
              className="btn"
              onClick={async () => {
                await api.POST("/api/v1/boards", { body: { name: "Main" } });
                queryClient.invalidateQueries({ queryKey: ["boards"] });
              }}
            >
              create one
            </button>
          </Empty>
        </Panel>
      </div>
    );
  }
  if (!detail) return <Empty>Loading…</Empty>;

  const onDragEnd = async (e: DragEndEvent) => {
    const taskId = String(e.active.id);
    const columnId = e.over ? String(e.over.id) : null;
    if (!columnId) return;
    const task = detail.tasks.find((t) => t.id === taskId);
    if (!task || task.column_id === columnId) return;
    queryClient.setQueryData(["boards", board.id], {
      ...detail,
      tasks: detail.tasks.map((t) =>
        t.id === taskId ? { ...t, column_id: columnId } : t,
      ),
    });
    await api.PATCH("/api/v1/tasks/{id}", {
      params: { path: { id: taskId } },
      body: { column_id: columnId },
    });
    queryClient.invalidateQueries({ queryKey: ["boards", board.id] });
  };

  const bust = () => queryClient.invalidateQueries({ queryKey: ["boards"] });

  const addTask = async (columnId: string, title: string) => {
    await api.POST("/api/v1/boards/{id}/tasks", {
      params: { path: { id: board.id } },
      body: { title, column_id: columnId },
    });
    bust();
  };
  const addColumn = async () => {
    const name = prompt("New column name");
    if (!name?.trim()) return;
    await api.POST("/api/v1/boards/{id}/columns", {
      params: { path: { id: board.id } },
      body: { name: name.trim() },
    });
    bust();
  };
  const renameColumn = async (colId: string, name: string) => {
    await api.PATCH("/api/v1/columns/{id}", { params: { path: { id: colId } }, body: { name } });
    bust();
  };
  const deleteColumn = async (colId: string) => {
    await api.DELETE("/api/v1/columns/{id}", { params: { path: { id: colId } } });
    bust();
  };
  const renameBoard = async () => {
    const name = prompt("Board name", detail.board.name);
    if (!name?.trim()) return;
    await api.PATCH("/api/v1/boards/{id}", { params: { path: { id: board.id } }, body: { name: name.trim() } });
    bust();
  };
  const deleteBoard = async () => {
    if (!confirm(`Delete board "${detail.board.name}" and everything on it?`)) return;
    await api.DELETE("/api/v1/boards/{id}", { params: { path: { id: board.id } } });
    bust();
  };

  return (
    <div className="nook-grid" style={{ gridTemplateColumns: "1fr" }}>
      <Panel
        title={`Board · ${detail.board.name}`}
        actions={
          <span style={{ display: "inline-flex", gap: 6, alignItems: "center" }}>
            <button className="btn small" onClick={addColumn} title="add column">
              <Plus size={12} /> column
            </button>
            <button className="btn small" onClick={renameBoard} title="rename board">
              <Pencil size={11} />
            </button>
            <button className="btn danger small" onClick={deleteBoard} title="delete board">
              <Trash2 size={11} />
            </button>
          </span>
        }
      >
        <div style={{ height: "100%", padding: 0 }}>
          <DndContext sensors={sensors} onDragEnd={onDragEnd}>
            <div className="board-columns">
              {detail.columns.map((c) => (
                <Column
                  key={c.id}
                  id={c.id}
                  name={c.name}
                  tasks={detail.tasks.filter((t) => t.column_id === c.id)}
                  onAdd={(title) => addTask(c.id, title)}
                  onStartWork={(t) =>
                    showNewWork({
                      taskId: t.id,
                      workspaceId: t.workspace_id ?? undefined,
                      worktree: true,
                    })
                  }
                  onRename={(n) => renameColumn(c.id, n)}
                  onDelete={() => deleteColumn(c.id)}
                  refresh={bust}
                />
              ))}
            </div>
          </DndContext>
        </div>
      </Panel>
    </div>
  );
}
