import React, { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useSearchParams } from "react-router-dom";
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
  Check,
  MoreHorizontal,
  X,
} from "lucide-react";
import { api, type TaskItem } from "@nookos/api";
import { Empty, Panel, Pill } from "@nookos/ui";
import { useNewWork } from "../newwork";
import { askChoice, askConfirm, askForm, askText, notify } from "../dialogs";
import { TaskDetail } from "../TaskDetail";
import { TaskMenu } from "../TaskMenu";
import { priorityMeta, priorityRank, previewText, PRIORITIES } from "../taskmeta";

function Card({
  task,
  onOpen,
  onMenu,
  selected,
  blocked,
}: {
  task: TaskItem;
  onOpen: () => void;
  onMenu: (anchor: { x: number; y: number }) => void;
  selected: boolean;
  blocked: boolean;
}) {
  const { attributes, listeners, setNodeRef, transform, isDragging } =
    useDraggable({ id: task.id });
  return (
    <div
      ref={setNodeRef}
      className={`board-card${selected ? " selected" : ""}${blocked ? " blocked" : ""}`}
      style={{
        transform: transform
          ? `translate(${transform.x}px, ${transform.y}px)`
          : undefined,
        opacity: isDragging ? 0.6 : 1,
        zIndex: isDragging ? 10 : undefined,
      }}
      onContextMenu={(e) => {
        e.preventDefault();
        onMenu({ x: e.clientX, y: e.clientY });
      }}
    >
      {/* Drag and open share this handle. The 4px activation distance is what
          separates them: a press that never moves is a click. */}
      <div
        className="card-title bright"
        {...attributes}
        {...listeners}
        onClick={onOpen}
      >
        {blocked && (
          <span className="card-blocked" title="blocked">
            ⊘
          </span>
        )}
        <span className="card-key mono">{task.key ?? ""}</span>
        {task.title}
      </div>
      {/* One button, revealed on hover. Right-clicking the card opens the same
          menu, so the gesture people already have works too. */}
      <button
        className="card-menu-btn"
        title="actions"
        onClick={(e) => {
          e.stopPropagation();
          const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
          onMenu({ x: r.right - 170, y: r.bottom + 4 });
        }}
      >
        <MoreHorizontal size={13} />
      </button>
      {/* Priority, labels and assignee on one dense row. A card is scanned in a
          column of twenty; anything that needs a second line to say "urgent"
          costs more than it tells you. */}
      {(task.priority || (task.labels ?? []).length > 0 || task.assignee_user_id) && (
        <div className="card-meta">
          {!!task.priority && (
            <span
              className="card-prio"
              style={{ color: priorityMeta(task.priority).color }}
              title={`priority: ${priorityMeta(task.priority).label}`}
            >
              {priorityMeta(task.priority).mark}
            </span>
          )}
          {(task.labels ?? []).map((l) => (
            <span
              key={l.id}
              className="card-label"
              style={{ borderColor: l.color, color: l.color }}
            >
              {l.name}
            </span>
          ))}
          {task.assignee_user_id && (
            <span className="card-assignee" title="claimed">
              ●
            </span>
          )}
        </div>
      )}
      {previewText(task.description) && (
        <div className="desc">{previewText(task.description)}</div>
      )}
    </div>
  );
}

function Column({
  id,
  name,
  type,
  tasks,
  onAdd,
  onRename,
  onDelete,
  onOpen,
  onMenu,
  selectedId,
  blockedIds,
}: {
  id: string;
  name: string;
  type?: string;
  tasks: TaskItem[];
  onAdd?: (title: string) => void;
  onRename: (name: string) => void;
  onDelete: () => void;
  onOpen: (id: string) => void;
  onMenu: (task: TaskItem, anchor: { x: number; y: number }) => void;
  selectedId: string | null;
  blockedIds: Set<string>;
}) {
  const { setNodeRef, isOver } = useDroppable({ id });
  return (
    <div className="board-column">
      <div className="nook-panel-title">
        <span>
          {name} <span className="faint">({tasks.length})</span>
          {/* The type is what automation targets, so it belongs on screen —
              otherwise "move to started" fails on a board whose columns look
              right and are typed wrong, with nothing to see. */}
          {type && type !== "unstarted" && (
            <span className="col-type faint mono"> {type}</span>
          )}
        </span>
        <span style={{ display: "inline-flex", gap: 3 }}>
          <button
            className="btn small"
            title="rename column"
            onClick={async () => {
              const n = await askText({
                title: "Rename column",
                value: name,
                confirmLabel: "rename",
              });
              if (n) onRename(n);
            }}
          >
            <Pencil size={11} />
          </button>
          <button
            className="btn small"
            title="delete column (and its tasks)"
            onClick={async () => {
              const ok = await askConfirm({
                title: `Delete column "${name}"`,
                description:
                  tasks.length > 0
                    ? `${tasks.length} task(s) in this column will be deleted too.`
                    : "This column is empty.",
                confirmLabel: "delete",
                danger: true,
              });
              if (ok) onDelete();
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
          <Card
            key={t.id}
            task={t}
            onOpen={() => onOpen(t.key ?? t.id)}
            onMenu={(anchor) => onMenu(t, anchor)}
            selected={selectedId === t.key || selectedId === t.id}
            blocked={blockedIds.has(t.id)}
          />
        ))}
        {onAdd && <Composer onAdd={onAdd} />}
      </div>
    </div>
  );
}

/**
 * "+ Create", then an empty card you type a title into.
 *
 * Filing a task used to open a modal with two fields. That is a lot of ceremony
 * for the thing people do most often — jot a title now, flesh it out later —
 * and the modal stole focus from the board you were reading. This is the
 * Bitbucket/Jira shape: the composer IS a card, in the column it will belong
 * to, and Enter files it and leaves you ready to type the next one.
 */
function Composer({ onAdd }: { onAdd: (title: string) => void }) {
  const [open, setOpen] = useState(false);
  const [title, setTitle] = useState("");
  const ref = React.useRef<HTMLTextAreaElement>(null);

  React.useEffect(() => {
    if (open) ref.current?.focus();
  }, [open]);

  const submit = () => {
    const t = title.trim();
    if (!t) return;
    onAdd(t);
    setTitle("");
    // Stay open: filing one task usually means filing three.
    ref.current?.focus();
  };

  if (!open) {
    return (
      <button className="composer-open" onClick={() => setOpen(true)}>
        <Plus size={13} /> Create
      </button>
    );
  }

  return (
    <div className="composer">
      <textarea
        ref={ref}
        className="composer-input"
        placeholder="What needs to be done?"
        value={title}
        rows={2}
        onChange={(e) => setTitle(e.target.value)}
        onKeyDown={(e) => {
          // Enter files it; Shift+Enter is a newline, because a title
          // occasionally wants one and losing the text to a stray keystroke is
          // worse than an extra modifier.
          if (e.key === "Enter" && !e.shiftKey) {
            e.preventDefault();
            submit();
          }
          if (e.key === "Escape") {
            setTitle("");
            setOpen(false);
          }
        }}
        onBlur={() => {
          // Clicking away with nothing typed means "never mind". With text in
          // it, keep it — silently discarding what somebody wrote is the worst
          // thing a composer can do.
          if (!title.trim()) setOpen(false);
        }}
      />
      <div className="composer-actions">
        <button
          className="btn small primary composer-save"
          onClick={submit}
          disabled={!title.trim()}
          title="create (Enter)"
        >
          <Check size={12} />
        </button>
      </div>
    </div>
  );
}

/** The filter strip. Drives the same query an agent's pick step uses. */
function Filters({
  labels,
  value,
  onChange,
}: {
  labels: { id: string; name: string; color: string }[];
  value: BoardFilter;
  onChange: (f: BoardFilter) => void;
}) {
  // Each label cycles include → exclude → off. Three states in one control,
  // because include and exclude are the same question asked twice and two rows
  // of chips would double the strip's height to say no more.
  const cycle = (name: string) => {
    if (value.label.includes(name)) {
      onChange({
        ...value,
        label: value.label.filter((l) => l !== name),
        not_label: [...value.not_label, name],
      });
    } else if (value.not_label.includes(name)) {
      onChange({ ...value, not_label: value.not_label.filter((l) => l !== name) });
    } else {
      onChange({ ...value, label: [...value.label, name] });
    }
  };
  const active =
    value.label.length > 0 ||
    value.not_label.length > 0 ||
    value.assignee !== "any" ||
    value.priority !== null ||
    value.blocked !== null;

  return (
    <div className="board-filters">
      <span className="faint small">labels</span>
      {labels.map((l) => {
        const inc = value.label.includes(l.name);
        const exc = value.not_label.includes(l.name);
        return (
          <button
            key={l.id}
            className={`task-chip ${inc ? "on" : ""} ${exc ? "off" : ""}`}
            style={inc ? { borderColor: l.color, color: l.color } : undefined}
            onClick={() => cycle(l.name)}
            title={inc ? "click to exclude" : exc ? "click to clear" : "click to require"}
          >
            {exc ? "−" : inc ? "+" : ""}
            {l.name}
          </button>
        );
      })}

      <span className="filter-sep" />
      <span className="faint small">assignee</span>
      <select
        className="task-select"
        value={value.assignee}
        onChange={(e) => onChange({ ...value, assignee: e.target.value as BoardFilter["assignee"] })}
      >
        <option value="any">any</option>
        <option value="none">unclaimed</option>
        <option value="me">mine</option>
      </select>

      <span className="faint small">priority</span>
      <select
        className="task-select"
        value={value.priority ?? ""}
        onChange={(e) =>
          onChange({ ...value, priority: e.target.value === "" ? null : Number(e.target.value) })
        }
      >
        <option value="">any</option>
        {PRIORITIES.map((p) => (
          <option key={p.value} value={p.value}>
            {p.label}
          </option>
        ))}
      </select>

      <button
        className={`task-chip ${value.blocked === false ? "on" : value.blocked === true ? "off" : ""}`}
        onClick={() =>
          onChange({
            ...value,
            blocked: value.blocked === null ? false : value.blocked === false ? true : null,
          })
        }
        title="cycle: any → unblocked only → blocked only"
      >
        {value.blocked === false ? "unblocked" : value.blocked === true ? "blocked" : "any block state"}
      </button>

      {active && (
        <button className="btn small" onClick={() => onChange(EMPTY_FILTER)}>
          clear
        </button>
      )}
    </div>
  );
}

export interface BoardFilter {
  label: string[];
  not_label: string[];
  assignee: "any" | "none" | "me";
  priority: number | null;
  blocked: boolean | null;
}

const EMPTY_FILTER: BoardFilter = {
  label: [],
  not_label: [],
  assignee: "any",
  priority: null,
  blocked: null,
};

export function BoardPage() {
  const queryClient = useQueryClient();
  const showNewWork = useNewWork((s) => s.show);
  // The open task lives in the URL, not in component state.
  //
  // `Copy link` hands out `/board?task=NOOK-42`, and an agent reporting "filed
  // NOOK-42" gives a human the same thing. Both were dead: nothing read the
  // parameter, so the link opened the board with no task showing. Keeping it in
  // the query string also makes Back close the modal, which is what every
  // browser user already expects.
  const [params, setParams] = useSearchParams();
  const openTask = params.get("task");
  const setOpenTask = (key: string | null) => {
    setParams(
      (prev) => {
        const next = new URLSearchParams(prev);
        if (key) next.set("task", key);
        else next.delete("task");
        return next;
      },
      // Opening a task is navigation; closing it should not need two Backs.
      { replace: !key },
    );
  };
  const [menu, setMenu] = useState<{
    task: TaskItem;
    anchor: { x: number; y: number };
  } | null>(null);
  const [filter, setFilter] = useState<BoardFilter>(EMPTY_FILTER);
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

  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
  const { data: labels } = useQuery({
    queryKey: ["labels"],
    queryFn: async () => (await api.GET("/api/v1/labels")).data ?? [],
  });

  // Blocked-ness is DERIVED from relations and column types, so the board
  // cannot work it out from the tasks it already holds — it would need every
  // task's relations. One query answers it for the whole board.
  const { data: blockedList } = useQuery({
    queryKey: ["tasks", "blocked", board?.id],
    queryFn: async () =>
      (
        await api.GET("/api/v1/tasks", {
          params: { query: { board: board!.id, is_blocked: true, limit: 200 } },
        })
      ).data ?? [],
    enabled: !!board,
  });

  // The filter strip drives the SAME query an agent's pick step uses, rather
  // than a parallel client-side filter. Two implementations of "which tasks
  // count" would drift, and the one a human sees is the one they use to decide
  // whether the loop will pick something up.
  const filterActive =
    filter.label.length > 0 ||
    filter.not_label.length > 0 ||
    filter.assignee !== "any" ||
    filter.priority !== null ||
    filter.blocked !== null;

  const { data: filtered } = useQuery({
    queryKey: ["tasks", "filtered", board?.id, filter, me?.user?.id],
    queryFn: async () =>
      (
        await api.GET("/api/v1/tasks", {
          params: {
            query: {
              board: board!.id,
              limit: 200,
              ...(filter.label.length ? { label: filter.label } : {}),
              ...(filter.not_label.length ? { not_label: filter.not_label } : {}),
              ...(filter.assignee === "none"
                ? { assignee: "none" }
                : filter.assignee === "me" && me?.user?.id
                  ? { assignee: me.user.id }
                  : {}),
              ...(filter.priority !== null ? { priority: filter.priority } : {}),
              ...(filter.blocked !== null ? { is_blocked: filter.blocked } : {}),
            },
          },
        })
      ).data ?? [],
    enabled: !!board && filterActive,
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

    // Land it at the BOTTOM of the target column. Carrying its old position
    // across would drop it into the middle of a column it has never been in,
    // at whatever index it happened to hold somewhere else — which reads as
    // the card jumping to a random place.
    const position =
      detail.tasks
        .filter((t) => t.column_id === columnId)
        .reduce((max, t) => Math.max(max, t.position), -1) + 1;

    queryClient.setQueryData(["boards", board.id], {
      ...detail,
      tasks: detail.tasks.map((t) =>
        t.id === taskId ? { ...t, column_id: columnId, position } : t,
      ),
    });
    await api.PATCH("/api/v1/tasks/{id}", {
      params: { path: { id: taskId } },
      body: { column_id: columnId, position },
    });
    queryClient.invalidateQueries({ queryKey: ["boards", board.id] });
  };

  const bust = () => queryClient.invalidateQueries({ queryKey: ["boards"] });

  const blockedIds = new Set((blockedList ?? []).map((t) => t.id));
  // When a filter is on, the API decides what is visible; otherwise show the
  // board. Sorted the way the API sorts so a human scanning a column sees the
  // same order the loop will pick in — urgent first, unset last, then oldest.
  const allowed = filterActive ? new Set((filtered ?? []).map((t) => t.id)) : null;
  // Ordered by `position` — what dragging a card writes.
  //
  // An earlier version sorted by priority here to mirror the API's pick order.
  // That was wrong twice over: dragging a card within a column became a no-op
  // you could not see, and cards silently rearranged themselves whenever a
  // priority changed. A board is a thing people arrange by hand; the pick order
  // belongs to the agent query, not to the furniture. Priority is SHOWN on the
  // card and filterable in the strip, and it does not move anything.
  const visible = detail.tasks
    .filter((t) => !allowed || allowed.has(t.id))
    .slice()
    .sort((a, b) => a.position - b.position || (a.created_at < b.created_at ? -1 : 1));

  const addTask = async (columnId: string, title: string) => {
    await api.POST("/api/v1/boards/{id}/tasks", {
      params: { path: { id: board.id } },
      body: { title, column_id: columnId },
    });
    bust();
  };
  const addColumn = async () => {
    const name = await askText({
      title: "New column",
      label: "Column name",
      placeholder: "In Review",
      confirmLabel: "add column",
    });
    if (!name) return;
    await api.POST("/api/v1/boards/{id}/columns", {
      params: { path: { id: board.id } },
      body: { name },
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
    const out = await askForm({
      title: "Board settings",
      // The key is normally immutable — it ends up in PR bodies and branch
      // names that a rename cannot reach back and fix. Editable anyway,
      // because a derived key is sometimes simply wrong, and living with it
      // forever is the worse outcome. Say what it costs and let them choose.
      description:
        "The key is the prefix in task codes like NOOK-42. Changing it breaks any link already written into a PR or a commit.",
      fields: [
        { name: "name", label: "Name", value: detail.board.name, required: true },
        {
          name: "key",
          label: "Key",
          value: detail.board.key ?? "",
          placeholder: "NOOK",
        },
      ],
      confirmLabel: "save",
    });
    if (!out?.name?.trim()) return;
    await api.PATCH("/api/v1/boards/{id}", {
      params: { path: { id: board.id } },
      body: { name: out.name.trim(), key: out.key?.trim() || null },
    });
    bust();
  };
  const deleteBoard = async () => {
    const ok = await askConfirm({
      title: `Delete board "${detail.board.name}"`,
      description: "Every column and task on this board is deleted. This cannot be undone.",
      confirmLabel: "delete board",
      danger: true,
    });
    if (!ok) return;
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
        <div className="board-body">
          <Filters labels={labels ?? []} value={filter} onChange={setFilter} />
          <div className="board-split">
            <DndContext sensors={sensors} onDragEnd={onDragEnd}>
              <div className="board-columns">
                {detail.columns.map((c) => (
                  <Column
                    key={c.id}
                    id={c.id}
                    name={c.name}
                    type={c.type}
                    tasks={visible.filter((t) => t.column_id === c.id)}
                    onAdd={(title) => addTask(c.id, title)}
                    onRename={(n) => renameColumn(c.id, n)}
                    onDelete={() => deleteColumn(c.id)}
                    onOpen={setOpenTask}
                    onMenu={(t, anchor) => setMenu({ task: t, anchor })}
                    selectedId={openTask}
                    blockedIds={blockedIds}
                  />
                ))}
              </div>
            </DndContext>
          </div>
        </div>
      </Panel>

      {openTask && (
        <TaskDetail
          taskId={openTask}
          columns={detail.columns}
          onClose={() => setOpenTask(null)}
          onMenu={(anchor) => {
            const t = detail.tasks.find(
              (x) => x.key === openTask || x.id === openTask,
            );
            if (t) setMenu({ task: t, anchor });
          }}
        />
      )}

      {menu && (
        <TaskMenu
          task={menu.task}
          columns={detail.columns}
          anchor={menu.anchor}
          onClose={() => setMenu(null)}
          onOpen={() => setOpenTask(menu.task.key ?? menu.task.id)}
          onStartWork={(t) =>
            showNewWork({
              taskId: t.id,
              workspaceId: t.workspace_id ?? undefined,
              worktree: true,
            })
          }
          refresh={bust}
        />
      )}
    </div>
  );
}
