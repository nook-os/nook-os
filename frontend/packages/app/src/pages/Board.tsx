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
  Archive,
  X,
  Zap,
} from "lucide-react";
import { api, type TaskItem } from "@nookos/api";
import { AutomationDialog } from "./BoardAutomation";
import { BoardBacklog } from "./BoardBacklog";
import { useBacklogSelection } from "./backlogSelection";
import { Empty, Panel, Pill, TypeBadge, TYPE_META } from "@nookos/ui";
import { useNewWork } from "../newwork";
import { askChoice, askConfirm, askForm, askText, notify } from "../dialogs";
import { TaskDetail } from "../TaskDetail";
import { TaskMenu } from "../TaskMenu";
import { priorityMeta, priorityRank, previewText, PRIORITIES } from "../taskmeta";

function Card({
  task,
  workspaceName,
  onOpen,
  onMenu,
  selected,
  blocked,
}: {
  task: TaskItem;
  /** The task's workspace, resolved to a name. Shown so a busy board tells you
   *  which repo each card belongs to — and which loop will build it. */
  workspaceName?: string;
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
      className={`board-card${selected ? " selected" : ""}${blocked ? " blocked" : ""}${
        task.archived_at ? " archived" : ""
      }`}
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
        {/* Non-default types are flagged inline so they are scannable across
            columns (AC-3); a plain `task` shows nothing, so a default board
            reads as before (AC-5). */}
        {task.type && task.type !== "task" && (
          <TypeBadge type={task.type} compact />
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
      {(task.priority ||
        (task.labels ?? []).length > 0 ||
        task.assignee_user_id ||
        workspaceName) && (
        <div className="card-meta">
          {workspaceName && (
            <span className="card-workspace" title={`workspace: ${workspaceName}`}>
              {workspaceName}
            </span>
          )}
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
  onArchiveCompleted,
  selectedId,
  blockedIds,
  wsName,
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
  /** Archive every live task in this column at once. Offered only for
   *  completed/canceled columns (AC-4). */
  onArchiveCompleted: () => void;
  selectedId: string | null;
  blockedIds: Set<string>;
  /** workspace id → name, so cards can label their repo without each fetching. */
  wsName: Map<string, string>;
}) {
  const { setNodeRef, isOver } = useDroppable({ id });
  // Bulk archive is finished-work cleanup only, and there must be something to
  // clean up.
  const isDone = type === "completed" || type === "canceled";
  const archivable = tasks.filter((t) => !t.archived_at).length;
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
          {isDone && archivable > 0 && (
            <button
              className="btn small"
              title={`archive all completed (${archivable})`}
              onClick={async () => {
                const ok = await askConfirm({
                  title: `Archive all completed (${archivable})`,
                  description: `Move ${archivable} finished task(s) off the board. They stay findable and can be unarchived.`,
                  confirmLabel: "archive",
                });
                if (ok) onArchiveCompleted();
              }}
            >
              <Archive size={11} />
            </button>
          )}
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
            workspaceName={t.workspace_id ? wsName.get(t.workspace_id) : undefined}
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

/** Debounced board search (MAIN-54). Controlled by the URL-backed `q`: it fires
 *  `onSearch` after the user pauses (not per keystroke), and re-syncs when `q`
 *  changes externally — e.g. the "clear" button — so the box empties with it.
 *  A ref keeps the debounced emitter stable across renders while always calling
 *  the latest handler. */
function BoardSearch({
  value,
  onSearch,
}: {
  value: string;
  onSearch: (q: string) => void;
}) {
  const [text, setText] = React.useState(value);
  React.useEffect(() => setText(value), [value]);
  const cb = React.useRef(onSearch);
  cb.current = onSearch;
  const timer = React.useRef<ReturnType<typeof setTimeout>>();
  return (
    <input
      className="board-search"
      type="search"
      value={text}
      placeholder="Search title, key, body…"
      aria-label="Search the board"
      onChange={(e) => {
        const q = e.target.value;
        setText(q);
        if (timer.current) clearTimeout(timer.current);
        timer.current = setTimeout(() => cb.current(q), 250);
      }}
    />
  );
}

/** The filter strip. Drives the same query an agent's pick step uses. */
function Filters({
  labels,
  workspaces,
  value,
  onChange,
}: {
  labels: { id: string; name: string; color: string }[];
  workspaces: { id: string; name: string }[];
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
  // Each type chip toggles membership; several selected are OR'd (any of them).
  const toggleType = (t: string) =>
    onChange({
      ...value,
      type: value.type.includes(t)
        ? value.type.filter((x) => x !== t)
        : [...value.type, t],
    });
  const active =
    value.label.length > 0 ||
    value.not_label.length > 0 ||
    value.type.length > 0 ||
    value.assignee !== "any" ||
    value.priority !== null ||
    value.blocked !== null ||
    value.q.length > 0;

  return (
    <div className="board-filters">
      <BoardSearch value={value.q} onSearch={(q) => onChange({ ...value, q })} />
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
      <span className="faint small">type</span>
      {TYPE_META.map((t) => {
        const on = value.type.includes(t.value);
        return (
          <button
            key={t.value}
            className={`task-chip type-chip ${on ? "on" : ""}`}
            aria-pressed={on}
            onClick={() => toggleType(t.value)}
            title={on ? `click to clear ${t.label}` : `filter to ${t.label}`}
          >
            <TypeBadge type={t.value} compact />
            {t.label}
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

      {workspaces.length > 1 && (
        <>
          <span className="filter-sep" />
          <span className="faint small">workspace</span>
          <select
            className="task-select"
            value={value.workspace ?? ""}
            onChange={(e) =>
              onChange({ ...value, workspace: e.target.value === "" ? null : e.target.value })
            }
          >
            <option value="">all</option>
            {workspaces.map((w) => (
              <option key={w.id} value={w.id}>
                {w.name}
              </option>
            ))}
          </select>
        </>
      )}

      <span className="filter-sep" />
      <button
        className={`task-chip ${value.showArchived ? "on" : ""}`}
        onClick={() => onChange({ ...value, showArchived: !value.showArchived })}
        title="show archived tasks (dimmed) in their columns"
      >
        show archived
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
  /** Issue types to include (OR'd); empty = any type. */
  type: string[];
  assignee: "any" | "none" | "me";
  priority: number | null;
  blocked: boolean | null;
  /** Workspace uuid, or null for all. Confines the board to one repo. */
  workspace: string | null;
  /** Reveal archived tasks (dimmed) in their columns. Default hidden. */
  showArchived: boolean;
  /** Free-text search across title, key, and description. Empty = no search. */
  q: string;
  /** Which Board-page tab is showing: the kanban `board` or the `backlog` list
   *  (MAIN-82). URL-addressable (`?view=backlog`) so it survives a refresh. */
  view: BoardView;
}

export type BoardView = "board" | "backlog";

/// Which tab a task belongs to (MAIN-82). A task lives in the Backlog tab when
/// its column is backlog-type OR it is an epic — epics never render on the
/// kanban tab (they are containers, shown flat in the backlog for now). Pure and
/// unit-tested so the split is one definition both tabs share.
export function isBacklogTask(
  columnType: string | undefined,
  taskType: string | null | undefined,
): boolean {
  return columnType === "backlog" || taskType === "epic";
}

/// The pick order the whole board reads in: priority first (unset last), then
/// oldest. Shared by the backlog list and every epic section (MAIN-82/83).
function pickOrder(a: TaskItem, b: TaskItem): number {
  return (
    priorityRank(a.priority ?? 0) - priorityRank(b.priority ?? 0) ||
    (a.created_at < b.created_at ? -1 : 1)
  );
}

export interface EpicSection {
  epic: TaskItem;
  /** Every child of the epic, on any column, in pick order. */
  children: TaskItem[];
  /** Children in a `completed` column, over the total — the progress count. */
  done: number;
  total: number;
}

export interface BacklogGroups {
  epics: EpicSection[];
  /** Parentless backlog-column tasks — the "No epic" section. */
  noEpic: TaskItem[];
}

/// Group the Backlog tab by epic (MAIN-83 AC-1): one section per epic on the
/// board (any column), each carrying ALL its children and a done/total count
/// derived from them; a final "No epic" bucket holds the parentless backlog
/// tasks. Pure and unit-tested so the grouping is one definition.
export function groupByEpic(
  tasks: TaskItem[],
  colTypeById: Map<string, string | undefined>,
): BacklogGroups {
  const childrenByEpic = new Map<string, TaskItem[]>();
  for (const t of tasks) {
    const pid = t.parent_task_id;
    if (pid) {
      const list = childrenByEpic.get(pid) ?? [];
      list.push(t);
      childrenByEpic.set(pid, list);
    }
  }
  const epics: EpicSection[] = tasks
    .filter((t) => t.type === "epic")
    .slice()
    .sort(pickOrder)
    .map((epic) => {
      const children = (childrenByEpic.get(epic.id) ?? []).slice().sort(pickOrder);
      const done = children.filter(
        (c) => colTypeById.get(c.column_id) === "completed",
      ).length;
      return { epic, children, done, total: children.length };
    });
  const noEpic = tasks
    .filter(
      (t) =>
        t.type !== "epic" &&
        !t.parent_task_id &&
        colTypeById.get(t.column_id) === "backlog",
    )
    .slice()
    .sort(pickOrder);
  return { epics, noEpic };
}

/// The epics a task may be filed under (MAIN-83 AC-4/AC-5): every epic on the
/// board except the task itself, in pick order. Pure and unit-tested.
export function epicOptions(tasks: TaskItem[], selfId: string): TaskItem[] {
  return tasks
    .filter((t) => t.type === "epic" && t.id !== selfId)
    .slice()
    .sort(pickOrder);
}

const EMPTY_FILTER: BoardFilter = {
  label: [],
  not_label: [],
  type: [],
  assignee: "any",
  priority: null,
  blocked: null,
  workspace: null,
  showArchived: false,
  q: "",
  view: "board",
};

// The filter lives in the URL so a filtered board is a link you can copy and
// reopen to the same view, reloading keeps it, and Back/forward step through
// filter changes — exactly like the `task` param. These two pure functions are
// the round-trip (unit-tested), and they only touch the filter keys, never
// `task`.
const FILTER_KEYS = [
  "label",
  "xlabel",
  "type",
  "assignee",
  "priority",
  "blocked",
  "ws",
  "archived",
  "q",
  "view",
] as const;

export function parseFilter(params: URLSearchParams): BoardFilter {
  const list = (k: string) =>
    (params.get(k) ?? "")
      .split(",")
      .map((s) => s.trim().toLowerCase())
      .filter(Boolean);
  const assignee = params.get("assignee");
  const priority = params.get("priority");
  const blocked = params.get("blocked");
  return {
    label: list("label"),
    not_label: list("xlabel"),
    type: list("type"),
    assignee: assignee === "none" || assignee === "me" ? assignee : "any",
    priority: priority !== null && priority !== "" ? Number(priority) : null,
    blocked: blocked === null ? null : blocked === "true",
    workspace: params.get("ws") || null,
    showArchived: params.get("archived") === "1",
    q: params.get("q") ?? "",
    view: params.get("view") === "backlog" ? "backlog" : "board",
  };
}

/** Apply a filter onto a URLSearchParams, clearing only the filter keys and
 *  leaving everything else (e.g. `task`) untouched. */
export function writeFilter(next: URLSearchParams, f: BoardFilter): URLSearchParams {
  for (const k of FILTER_KEYS) next.delete(k);
  if (f.label.length) next.set("label", f.label.join(","));
  if (f.not_label.length) next.set("xlabel", f.not_label.join(","));
  if (f.type.length) next.set("type", f.type.join(","));
  if (f.assignee !== "any") next.set("assignee", f.assignee);
  if (f.priority !== null) next.set("priority", String(f.priority));
  if (f.blocked !== null) next.set("blocked", String(f.blocked));
  if (f.workspace) next.set("ws", f.workspace);
  if (f.showArchived) next.set("archived", "1");
  if (f.q) next.set("q", f.q);
  if (f.view === "backlog") next.set("view", "backlog");
  return next;
}

/** Serialize a filter to a fresh URLSearchParams — the half of the round-trip
 *  that `parseFilter` inverts. */
export function serializeFilter(f: BoardFilter): URLSearchParams {
  return writeFilter(new URLSearchParams(), f);
}

/** Whether a task shows on the board given the archive toggle: archived tasks
 *  are hidden unless "show archived" is on (AC-5). */
export function showsUnderArchive(
  showArchived: boolean,
  archivedAt: string | null | undefined,
): boolean {
  return showArchived || !archivedAt;
}

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
  // The filter lives in the URL, like the open task. A filter change is a
  // history entry (push) so Back/forward walk through them; the `task` param is
  // preserved across filter edits by `writeFilter`.
  const filter = React.useMemo(() => parseFilter(params), [params]);
  const setFilter = (f: BoardFilter) =>
    setParams((prev) => writeFilter(new URLSearchParams(prev), f));

  // Bulk selection on the Backlog tab (MAIN-123). It lives in a store so the
  // toolbar count and later bulk actions read one source. It must NEVER outlive
  // the rows it points at: clear it whenever the filter changes (rows appear or
  // vanish) or the backlog tab is left (AC-5) — a stale selection can't be
  // allowed to act on rows the user can no longer see.
  const selected = useBacklogSelection((s) => s.selected);
  const toggleSelect = useBacklogSelection((s) => s.toggle);
  const clearSelection = useBacklogSelection((s) => s.clear);
  const filterKey = serializeFilter(filter).toString();
  React.useEffect(() => {
    if (filter.view !== "backlog") clearSelection();
  }, [filter.view, clearSelection]);
  React.useEffect(() => {
    // Any filter edit invalidates the selection — the visible row set changed.
    clearSelection();
  }, [filterKey, clearSelection]);
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
  // Workspaces, to turn each task's `workspace_id` into a name — one fetch for
  // the whole board rather than one per card.
  const { data: workspaces } = useQuery({
    queryKey: ["workspaces"],
    queryFn: async () => (await api.GET("/api/v1/workspaces")).data ?? [],
  });
  const wsName = React.useMemo(
    () => new Map((workspaces ?? []).map((w) => [w.id, w.name])),
    [workspaces],
  );

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
    filter.type.length > 0 ||
    filter.assignee !== "any" ||
    filter.priority !== null ||
    filter.blocked !== null ||
    filter.workspace !== null ||
    filter.q.length > 0;

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
              ...(filter.type.length ? { type: filter.type } : {}),
              ...(filter.assignee === "none"
                ? { assignee: "none" }
                : filter.assignee === "me" && me?.user?.id
                  ? { assignee: me.user.id }
                  : {}),
              ...(filter.priority !== null ? { priority: filter.priority } : {}),
              ...(filter.blocked !== null ? { is_blocked: filter.blocked } : {}),
              ...(filter.workspace ? { workspace: filter.workspace } : {}),
              ...(filter.q ? { q: filter.q } : {}),
              // When showing archived, the server filter must include them too,
              // or a filtered view would drop the archived cards the toggle is
              // meant to reveal.
              ...(filter.showArchived ? { archived: true } : {}),
              // On the Backlog tab, the server would exclude backlog-column tasks
              // by default (MAIN-80), blanking the very view being filtered — so
              // ask for them (MAIN-82 AC-5). The kanban tab does not.
              ...(filter.view === "backlog" ? { backlog: true } : {}),
            },
          },
        })
      ).data ?? [],
    enabled: !!board && filterActive,
  });

  // Deep link to a backlog task → show the Backlog tab (MAIN-82 AC-4). Only when
  // the tab was not already chosen explicitly (no `view` in the URL), so a manual
  // Board-tab choice is respected. `replace` so it does not add a history entry.
  React.useEffect(() => {
    if (!detail || !openTask || params.has("view")) return;
    const t = detail.tasks.find((x) => x.key === openTask || x.id === openTask);
    if (!t) return;
    const colType = detail.columns.find((c) => c.id === t.column_id)?.type;
    if (isBacklogTask(colType, t.type)) {
      setParams(
        (prev) => {
          const next = new URLSearchParams(prev);
          next.set("view", "backlog");
          return next;
        },
        { replace: true },
      );
    }
  }, [detail, openTask, params, setParams]);

  const refresh = () => queryClient.invalidateQueries({ queryKey: ["boards"] });

  // Board-automation panel toggle. Declared with the other hooks, ABOVE every
  // early return below: a `useState` after `if (!board) return` is a
  // Rules-of-Hooks violation — when the boards query re-resolves (e.g. a card's
  // type change busts ["boards"]) the hook count changes and BoardPage throws
  // "rendered more hooks than during the previous render", white-screening the
  // board (MAIN-99).
  const [showAutomation, setShowAutomation] = useState(false);

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
    // Archived work is off the board unless the toggle is on (AC-5).
    .filter((t) => showsUnderArchive(filter.showArchived, t.archived_at))
    .slice()
    .sort((a, b) => a.position - b.position || (a.created_at < b.created_at ? -1 : 1));

  // The kanban tab shows only real workflow columns; backlog-type columns move
  // to the Backlog tab (MAIN-82 AC-1). Both are fed by `detail` — one fetch.
  const colTypeById = new Map(detail.columns.map((c) => [c.id, c.type]));
  const kanbanColumns = detail.columns.filter((c) => c.type !== "backlog");
  const backlogColumn = detail.columns.find((c) => c.type === "backlog");
  const unstartedColumn = detail.columns.find((c) => c.type === "unstarted");
  // The backlog list: backlog-column tasks and epics, in pick order (priority
  // first — unset last — then oldest), the same order the agent pick uses (AC-2).
  const backlogTasks = visible
    .filter((t) => isBacklogTask(colTypeById.get(t.column_id), t.type))
    .slice()
    .sort(
      (a, b) =>
        priorityRank(a.priority ?? 0) - priorityRank(b.priority ?? 0) ||
        (a.created_at < b.created_at ? -1 : 1),
    );

  // The Backlog tab grouped by epic (MAIN-83). An epic's children stay visible
  // UNDER their epic even when archived/done — dimmed, not hidden — so an epic
  // keeps showing the work it has finished until the epic itself is archived or
  // its queue is done. Everything else (the epics themselves, the "No epic"
  // bucket, the kanban board) still respects the archive toggle via `visible`.
  // So group over `visible` PLUS any archived task that is a child of an epic and
  // is not already in `visible` (it would be, with the toggle on).
  const epicIds = new Set(detail.tasks.filter((t) => t.type === "epic").map((t) => t.id));
  const inVisible = new Set(visible.map((t) => t.id));
  const archivedEpicChildren = detail.tasks.filter(
    (t) =>
      !inVisible.has(t.id) &&
      t.archived_at &&
      t.parent_task_id &&
      epicIds.has(t.parent_task_id),
  );
  const backlogGroups = groupByEpic([...visible, ...archivedEpicChildren], colTypeById);
  const epics = detail.tasks.filter((t) => t.type === "epic");

  const addTask = async (columnId: string, title: string) => {
    await api.POST("/api/v1/boards/{id}/tasks", {
      params: { path: { id: board.id } },
      body: { title, column_id: columnId },
    });
    bust();
  };
  // A new epic (MAIN-83 AC-3): a `type='epic'` task in the backlog column.
  const addEpic = async (title: string) => {
    if (!backlogColumn) return;
    await api.POST("/api/v1/boards/{id}/tasks", {
      params: { path: { id: board.id } },
      body: { title, column_id: backlogColumn.id, type: "epic" },
    });
    bust();
  };
  // A child of an epic (MAIN-83 AC-3): filed into the backlog with `parent` preset.
  const addChild = async (epicId: string, title: string) => {
    if (!backlogColumn) return;
    await api.POST("/api/v1/boards/{id}/tasks", {
      params: { path: { id: board.id } },
      body: { title, column_id: backlogColumn.id, parent: epicId },
    });
    bust();
  };
  // Send a backlog task to the board: move it to the unstarted column, no node
  // assigned (AC-3). Dispatch instead lets the scheduler pick a node.
  const sendToBoard = async (taskId: string) => {
    if (!unstartedColumn) return;
    await api.PATCH("/api/v1/tasks/{id}", {
      params: { path: { id: taskId } },
      body: { column_id: unstartedColumn.id },
    });
    bust();
  };
  const dispatchTask = async (taskId: string) => {
    await api.POST("/api/v1/tasks/{id}/dispatch", { params: { path: { id: taskId } } });
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
  const archiveCompleted = async (colId: string) => {
    await api.POST("/api/v1/columns/{id}/archive-completed", {
      params: { path: { id: colId } },
    });
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
            <button
              className="btn small"
              onClick={() => setShowAutomation(true)}
              title="automation"
            >
              <Zap size={11} /> automation
            </button>
            <button className="btn danger small" onClick={deleteBoard} title="delete board">
              <Trash2 size={11} />
            </button>
          </span>
        }
      >
        <div className="board-body">
          {/* Board / Backlog tabs (MAIN-82 AC-1). The kanban is the workflow;
              the backlog is the refinement queue that used to crowd it. */}
          <div className="board-tabs" role="tablist" aria-label="Board views">
            <button
              role="tab"
              aria-selected={filter.view === "board"}
              className={`board-tab${filter.view === "board" ? " active" : ""}`}
              onClick={() => setFilter({ ...filter, view: "board" })}
            >
              Board
            </button>
            <button
              role="tab"
              aria-selected={filter.view === "backlog"}
              className={`board-tab${filter.view === "backlog" ? " active" : ""}`}
              onClick={() => setFilter({ ...filter, view: "backlog" })}
            >
              Backlog
              {backlogTasks.length > 0 && (
                <span className="board-tab-count">{backlogTasks.length}</span>
              )}
            </button>
          </div>
          <Filters
            labels={labels ?? []}
            workspaces={workspaces ?? []}
            value={filter}
            onChange={setFilter}
          />
          {/* Distinct from a genuinely empty board: a filter/search is on and
              nothing matched, rather than "this board has no tasks" (AC-4). */}
          {filterActive && visible.length === 0 && (
            <div className="board-no-matches faint small">
              {filter.q
                ? `No tasks match “${filter.q}”.`
                : "No tasks match these filters."}
            </div>
          )}
          {filter.view === "backlog" ? (
            <BoardBacklog
              groups={backlogGroups}
              colTypeById={colTypeById}
              wsName={wsName}
              activeId={openTask}
              selected={selected}
              blockedIds={blockedIds}
              canSendToBoard={!!unstartedColumn}
              onAddEpic={addEpic}
              onAddChild={addChild}
              onAddBacklog={(title) => backlogColumn && addTask(backlogColumn.id, title)}
              onOpen={setOpenTask}
              onMenu={(t, anchor) => setMenu({ task: t, anchor })}
              onToggleSelect={toggleSelect}
              onSendToBoard={sendToBoard}
              onDispatch={dispatchTask}
            />
          ) : (
            <div className="board-split">
              <DndContext sensors={sensors} onDragEnd={onDragEnd}>
                <div className="board-columns">
                  {kanbanColumns.map((c) => (
                    <Column
                      key={c.id}
                      id={c.id}
                      name={c.name}
                      type={c.type}
                      // Epics never render on the kanban tab — they show only in
                      // the Backlog list, flat (AC-5).
                      tasks={visible.filter(
                        (t) => t.column_id === c.id && t.type !== "epic",
                      )}
                      onAdd={(title) => addTask(c.id, title)}
                      onRename={(n) => renameColumn(c.id, n)}
                      onDelete={() => deleteColumn(c.id)}
                      onOpen={setOpenTask}
                      onMenu={(t, anchor) => setMenu({ task: t, anchor })}
                      onArchiveCompleted={() => archiveCompleted(c.id)}
                      selectedId={openTask}
                      blockedIds={blockedIds}
                      wsName={wsName}
                    />
                  ))}
                </div>
              </DndContext>
            </div>
          )}
        </div>
      </Panel>

      {showAutomation && (
        <AutomationDialog
          boardId={board.id}
          boardName={detail.board.name}
          automation={detail.board.automation}
          onClose={() => setShowAutomation(false)}
          onSaved={bust}
        />
      )}

      {openTask && (
        <TaskDetail
          taskId={openTask}
          columns={detail.columns}
          epics={epics}
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
          epics={epics}
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
