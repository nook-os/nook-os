import React, { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useSearchParams } from "react-router-dom";
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
  Layers,
  Pencil,
  Sparkles,
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
import { api, type TaskItem, type LoopJob } from "@nookos/api";
import { AutomationDialog } from "./BoardAutomation";
import { BoardBacklog } from "./BoardBacklog";
import { NewTicketModal } from "../NewTicketModal";
import { summarizeBulk, useBacklogSelection } from "./backlogSelection";
import {
  Empty,
  Panel,
  Pill,
  TypeBadge,
  TypeSelect,
  useAnchoredMenu,
  VisibilityBadge,
  VISIBILITY_META,
} from "@nookos/ui";
import { useNewWork } from "../newwork";
import { askChoice, askConfirm, askForm, askText, notify } from "../dialogs";
import { TaskDetail } from "../TaskDetail";
import { taskMenuItems } from "../TaskMenu";
import {
  ContextMenuRegion,
  useContextMenuApi,
  type ContextMenuItem,
} from "../contextMenu";
import { fetchTaskJobs, taskJobsKey } from "../loop";
import { priorityMeta, priorityRank, previewText, PRIORITIES } from "../taskmeta";

function Card({
  task,
  workspaceName,
  onOpen,
  menuItems,
  selected,
  blocked,
  hit = false,
}: {
  task: TaskItem;
  /** The task's workspace, resolved to a name. Shown so a busy board tells you
   *  which repo each card belongs to — and which loop will build it. */
  workspaceName?: string;
  onOpen: () => void;
  /** The task's action-menu items, for the shared context-menu primitive
   *  (MAIN-168): right-click opens them via the card's region, the three-dots
   *  button opens them via `openAt`. */
  menuItems: () => ContextMenuItem[];
  selected: boolean;
  blocked: boolean;
  /** This card is the exact-key search hit — highlighted + scrolled into view
   *  (MAIN-181 AC-3). */
  hit?: boolean;
}) {
  const { openAt } = useContextMenuApi();
  const { attributes, listeners, setNodeRef, transform, isDragging } =
    useDraggable({ id: task.id });
  // Compose dnd-kit's draggable ref with our own so an exact-key hit can be
  // scrolled into view (AC-3) without losing drag behaviour.
  const node = React.useRef<HTMLElement | null>(null);
  const setRefs = (el: HTMLElement | null) => {
    node.current = el;
    setNodeRef(el);
  };
  React.useEffect(() => {
    if (hit) node.current?.scrollIntoView({ block: "center", behavior: "smooth" });
  }, [hit]);
  // Right-click anywhere on the card opens the task menu through the shared
  // primitive's region (MAIN-168). `display: contents` keeps the wrapper out of
  // the board-cards flex layout.
  return (
    <ContextMenuRegion items={menuItems} style={{ display: "contents" }}>
    <div
      ref={setRefs}
      className={`board-card${selected ? " selected" : ""}${hit ? " hit" : ""}${blocked ? " blocked" : ""}${
        task.archived_at ? " archived" : ""
      }`}
      style={{
        transform: transform
          ? `translate(${transform.x}px, ${transform.y}px)`
          : undefined,
        opacity: isDragging ? 0.6 : 1,
        zIndex: isDragging ? 10 : undefined,
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
        {/* Visibility is flagged inline only when it is NOT the default `team`
            (MAIN-103) — a private/org card is scannable across columns, while a
            plain team card shows nothing, so a default board reads as before,
            exactly like the type badge above. */}
        {task.visibility && task.visibility !== "team" && (
          <VisibilityBadge visibility={task.visibility} compact />
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
          openAt(r.right - 170, r.bottom + 4, menuItems());
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
    </ContextMenuRegion>
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
  menuItems,
  onArchiveCompleted,
  selectedId,
  hitId = null,
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
  /** Build a task's action-menu items for the shared primitive (MAIN-168). */
  menuItems: (task: TaskItem) => ContextMenuItem[];
  /** Archive every live task in this column at once. Offered only for
   *  completed/canceled columns (AC-4). */
  onArchiveCompleted: () => void;
  selectedId: string | null;
  /** The exact-key search hit's task id — the card to highlight/scroll (AC-3). */
  hitId?: string | null;
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
            menuItems={() => menuItems(t)}
            selected={selectedId === t.key || selectedId === t.id}
            hit={hitId === t.id}
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

/** The filter strip (MAIN-110): compact by default — search box, one removable
 *  chip per active filter, a `Filters` button that opens a popover holding every
 *  control, and a clear-all. Drives the same query an agent's pick step uses. */
function Filters({
  labels,
  workspaces,
  members,
  epics,
  value,
  onChange,
}: {
  labels: { id: string; name: string; color: string }[];
  workspaces: { id: string; name: string }[];
  /** Tenant members for the specific-person assignee filter (MAIN-111). */
  members: { id: string; name: string }[];
  /** Board epics, in pick order, for the epic filter (MAIN-111). */
  epics: { id: string; key: string }[];
  value: BoardFilter;
  onChange: (f: BoardFilter) => void;
}) {
  const [open, setOpen] = React.useState(false);
  const [labelQuery, setLabelQuery] = React.useState("");
  // The popover is anchored outside the strip's overflow, like the other board
  // pickers, so it is not clipped by the panel.
  const { hostRef, portal } = useAnchoredMenu(open, () => setOpen(false), {
    height: 420,
  });
  // Esc closes (outside-click is handled by the anchored-menu hook) — AC-2.
  React.useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open]);

  // Each label cycles include → exclude → off. Three states in one control,
  // because include and exclude are the same question asked twice.
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
  // Same shape for visibility: each chip toggles membership, several are OR'd.
  const toggleVisibility = (v: string) =>
    onChange({
      ...value,
      visibility: value.visibility.includes(v)
        ? value.visibility.filter((x) => x !== v)
        : [...value.visibility, v],
    });

  const chips = activeChips(value, workspaces, members, epics);
  const anyActive = isFilterActive(value);
  // Clear-all resets to empty but keeps the current TAB (AC-4); the open task
  // rides in a separate URL key that writeFilter never touches.
  const clearAll = () => onChange({ ...EMPTY_FILTER, view: value.view });

  // A long label list gets a search box inside the popover (AC-5).
  const q = labelQuery.trim().toLowerCase();
  const shownLabels =
    labels.length > 10 && q ? labels.filter((l) => l.name.toLowerCase().includes(q)) : labels;

  return (
    <div className="board-filters">
      <BoardSearch value={value.q} onSearch={(qq) => onChange({ ...value, q: qq })} />

      {/* One removable chip per active filter (AC-3). An excluded label reads
          distinctly (−name, struck through) from an included one. */}
      {chips.map((c) => (
        <button
          key={c.key}
          className={`task-chip filter-chip ${c.negated ? "off" : "on"}`}
          onClick={() => onChange(c.next)}
          title="remove this filter"
        >
          {c.negated ? "−" : ""}
          {c.label}
          <span className="filter-chip-x">×</span>
        </button>
      ))}

      <div ref={hostRef} style={{ display: "inline-flex" }}>
        <button
          className="btn small"
          aria-expanded={open}
          aria-haspopup="dialog"
          onClick={() => setOpen((o) => !o)}
        >
          Filters
          {chips.length > 0 && <span className="filter-badge">{chips.length}</span>}
        </button>
      </div>

      {anyActive && (
        <button className="btn small" onClick={clearAll}>
          clear
        </button>
      )}

      {portal(
        <div
          className="filters-popover"
          role="dialog"
          aria-label="Filters"
          // Keep clicks inside from bubbling to a listener that might close it.
          onMouseDown={(e) => e.stopPropagation()}
        >
          <div className="filters-group">
            <div className="filters-group-head">
              <span className="faint small">labels</span>
              <span className="faint small filters-legend">+ include · − exclude · click again to clear</span>
            </div>
            {labels.length > 10 && (
              <input
                className="board-search filters-label-search"
                type="search"
                value={labelQuery}
                placeholder="Filter labels…"
                aria-label="Filter the label list"
                onChange={(e) => setLabelQuery(e.target.value)}
              />
            )}
            <div className="filters-chips">
              {shownLabels.map((l) => {
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
              {shownLabels.length === 0 && <span className="faint small">no matching labels</span>}
            </div>
          </div>

          {/* The same control the task modal uses (MAIN-174), in its
              multiple-select mode: several types are OR'd, exactly as the chip
              row before it did. */}
          <div className="filters-group filters-row">
            <label className="filters-field">
              <span className="faint small">type</span>
              <TypeSelect
                multiple
                value={value.type}
                ariaLabel="filter by work type"
                onChange={(types) => onChange({ ...value, type: types })}
              />
            </label>
          </div>

          <div className="filters-group">
            <span className="faint small">visibility</span>
            <div className="filters-chips">
              {VISIBILITY_META.map((v) => {
                const on = value.visibility.includes(v.value);
                return (
                  <button
                    key={v.value}
                    className={`task-chip type-chip ${on ? "on" : ""}`}
                    aria-pressed={on}
                    onClick={() => toggleVisibility(v.value)}
                    title={on ? `click to clear ${v.label}` : `filter to ${v.label}`}
                  >
                    <VisibilityBadge visibility={v.value} compact />
                    {v.label}
                  </button>
                );
              })}
            </div>
          </div>

          <div className="filters-group filters-row">
            <label className="filters-field">
              <span className="faint small">assignee</span>
              <select
                className="task-select"
                value={value.assignee}
                onChange={(e) => onChange({ ...value, assignee: e.target.value })}
              >
                <option value="any">any</option>
                <option value="none">unclaimed</option>
                <option value="me">mine</option>
                {/* Every tenant member, by display name (MAIN-111 AC-1). */}
                {members.map((m) => (
                  <option key={m.id} value={m.id}>
                    {m.name}
                  </option>
                ))}
              </select>
            </label>

            <label className="filters-field">
              <span className="faint small">epic</span>
              <select
                className="task-select"
                value={value.epic ?? ""}
                onChange={(e) =>
                  onChange({ ...value, epic: e.target.value === "" ? null : e.target.value })
                }
              >
                <option value="">all</option>
                {epics.map((ep) => (
                  <option key={ep.id} value={ep.id}>
                    {ep.key}
                  </option>
                ))}
              </select>
            </label>

            <label className="filters-field">
              <span className="faint small">priority</span>
              <select
                className="task-select"
                value={value.priority ?? ""}
                onChange={(e) =>
                  onChange({
                    ...value,
                    priority: e.target.value === "" ? null : Number(e.target.value),
                  })
                }
              >
                <option value="">any</option>
                {PRIORITIES.map((p) => (
                  <option key={p.value} value={p.value}>
                    {p.label}
                  </option>
                ))}
              </select>
            </label>

            {workspaces.length > 1 && (
              <label className="filters-field">
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
              </label>
            )}
          </div>

          <div className="filters-group filters-row">
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
              {value.blocked === false
                ? "unblocked"
                : value.blocked === true
                  ? "blocked"
                  : "any block state"}
            </button>

            <button
              className={`task-chip ${value.showArchived ? "on" : ""}`}
              onClick={() => onChange({ ...value, showArchived: !value.showArchived })}
              title="show archived tasks (dimmed) in their columns"
            >
              show archived
            </button>
          </div>
        </div>,
        "filters-popover-host",
      )}
    </div>
  );
}

export interface BoardFilter {
  label: string[];
  not_label: string[];
  /** Issue types to include (OR'd); empty = any type. */
  type: string[];
  /** Visibilities to include (OR'd); empty = any visibility (MAIN-103). */
  visibility: string[];
  /** `any` · `none` (unclaimed) · `me` · a specific user's uuid (MAIN-111). */
  assignee: string;
  priority: number | null;
  blocked: boolean | null;
  /** An epic's task uuid to confine the board to its children, or null for all
   *  (MAIN-111). Rides the server `parent` query param. */
  epic: string | null;
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

/// Every task type the Backlog tab lists, INCLUDING `epic`. Listing epic
/// explicitly opts a filtered fetch out of the server's default epic-exclusion
/// (MAIN-80), so a backlog search finds epics (MAIN-181 AC-1).
export const BACKLOG_TYPES = ["task", "bug", "story", "chore", "epic"] as const;

/// The `type` query param a filtered board/backlog fetch should send (MAIN-181
/// AC-1): an explicit type filter always wins; with none, the Backlog tab asks
/// for all types incl. epic (so epics stay searchable), and the kanban tab sends
/// nothing (epics never render there). `undefined` means "omit the param". Pure
/// and exported for the query-composition test (AC-5).
export function searchTypeParam(filter: BoardFilter): string[] | undefined {
  if (filter.type.length) return filter.type;
  if (filter.view === "backlog") return [...BACKLOG_TYPES];
  return undefined;
}

/// The id of the task whose display key EXACTLY equals the query (case-
/// insensitive), or null — the exact-key search hit to highlight and scroll to
/// (MAIN-181 AC-3). A partial query (not a whole key) matches nothing here; the
/// server ILIKE still lists partial matches, this only drives the jump-to. Pure
/// and exported for the test.
export function exactKeyMatch(tasks: TaskItem[], q: string): string | null {
  const key = q.trim().toLowerCase();
  if (!key) return null;
  return tasks.find((t) => (t.key ?? "").toLowerCase() === key)?.id ?? null;
}

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

/// The epic rows to add to a FILTERED backlog grouping so a matching child shows
/// under its epic's header even when the epic itself did not match the search
/// (MAIN-181 AC-2). Returns the epics that are (a) not already in `visible` and
/// (b) referenced as a parent by some visible task. Pure and unit-tested.
export function matchedEpicHeaders(visible: TaskItem[], allTasks: TaskItem[]): TaskItem[] {
  const inVisible = new Set(visible.map((t) => t.id));
  const parents = new Set(
    visible.map((t) => t.parent_task_id).filter((p): p is string => !!p),
  );
  return allTasks.filter(
    (t) => t.type === "epic" && !inVisible.has(t.id) && parents.has(t.id),
  );
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
  visibility: [],
  assignee: "any",
  priority: null,
  blocked: null,
  epic: null,
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
  "vis",
  "assignee",
  "priority",
  "blocked",
  "epic",
  "ws",
  "archived",
  "q",
  "view",
] as const;

/** A uuid, so a person/epic filter value from the URL is recognised as one
 *  (rather than coerced away) without accepting arbitrary junk (MAIN-111). */
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

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
    visibility: list("vis"),
    // `none`/`me` keep their meaning; a uuid is a specific person (MAIN-111);
    // anything else (garbage) falls back to `any`. A uuid naming a user who no
    // longer exists is kept — the board renders empty with a removable chip
    // rather than crashing (AC-6).
    assignee:
      assignee === "none" || assignee === "me"
        ? assignee
        : assignee && UUID_RE.test(assignee)
          ? assignee
          : "any",
    priority: priority !== null && priority !== "" ? Number(priority) : null,
    blocked: blocked === null ? null : blocked === "true",
    epic: (() => {
      const e = params.get("epic");
      return e && UUID_RE.test(e) ? e : null;
    })(),
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
  if (f.visibility.length) next.set("vis", f.visibility.join(","));
  if (f.assignee !== "any") next.set("assignee", f.assignee);
  if (f.priority !== null) next.set("priority", String(f.priority));
  if (f.blocked !== null) next.set("blocked", String(f.blocked));
  if (f.epic) next.set("epic", f.epic);
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

/** One active filter, as an inline chip: a display label, whether it is an
 *  exclusion (an excluded label reads `−name` and struck through), and the
 *  filter that results from removing JUST this one (MAIN-110 AC-3). */
export interface FilterChip {
  key: string;
  label: string;
  negated?: boolean;
  next: BoardFilter;
}

/** Every active filter as its own chip, in a stable order (MAIN-110 AC-1/AC-3).
 *  Search is deliberately NOT a chip — it has its own always-visible box and is
 *  excluded from the count (AC-2). `workspaces` only resolves the workspace
 *  chip's display name; the chip is present whenever a workspace is set. Pure,
 *  so "which chips are active" / the active count are unit-tested. */
export function activeChips(
  f: BoardFilter,
  workspaces: { id: string; name: string }[],
  members: { id: string; name: string }[] = [],
  epics: { id: string; key: string }[] = [],
): FilterChip[] {
  const chips: FilterChip[] = [];
  for (const l of f.label)
    chips.push({ key: `label:${l}`, label: l, next: { ...f, label: f.label.filter((x) => x !== l) } });
  for (const l of f.not_label)
    chips.push({
      key: `xlabel:${l}`,
      label: l,
      negated: true,
      next: { ...f, not_label: f.not_label.filter((x) => x !== l) },
    });
  for (const t of f.type)
    chips.push({ key: `type:${t}`, label: t, next: { ...f, type: f.type.filter((x) => x !== t) } });
  for (const v of f.visibility)
    chips.push({ key: `vis:${v}`, label: v, next: { ...f, visibility: f.visibility.filter((x) => x !== v) } });
  if (f.assignee !== "any")
    chips.push({
      key: "assignee",
      label:
        f.assignee === "me"
          ? "mine"
          : f.assignee === "none"
            ? "unclaimed"
            : // A specific person: their display name, or a fallback if the
              // user no longer exists — the chip still removes cleanly (AC-6).
              (members.find((m) => m.id === f.assignee)?.name ?? "unknown user"),
      next: { ...f, assignee: "any" },
    });
  if (f.epic)
    chips.push({
      key: "epic",
      label: epics.find((e) => e.id === f.epic)?.key ?? "unknown epic",
      next: { ...f, epic: null },
    });
  if (f.priority !== null)
    chips.push({
      key: "priority",
      label: PRIORITIES.find((p) => p.value === f.priority)?.label ?? `priority ${f.priority}`,
      next: { ...f, priority: null },
    });
  if (f.blocked !== null)
    chips.push({
      key: "blocked",
      label: f.blocked ? "blocked" : "unblocked",
      next: { ...f, blocked: null },
    });
  if (f.workspace)
    chips.push({
      key: "ws",
      label: workspaces.find((w) => w.id === f.workspace)?.name ?? "workspace",
      next: { ...f, workspace: null },
    });
  if (f.showArchived)
    chips.push({ key: "archived", label: "archived", next: { ...f, showArchived: false } });
  return chips;
}

/** Whether ANY filter is active — the clear-all gate. Unlike the old inline
 *  check it counts a workspace-only or archived-only filter (MAIN-110 AC-4), and
 *  it includes search because clear-all resets that too. */
export function isFilterActive(f: BoardFilter): boolean {
  return activeChips(f, []).length > 0 || f.q.length > 0;
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
  const navigate = useNavigate();
  const { openAt } = useContextMenuApi();
  const showNewWork = useNewWork((s) => s.show);
  // The open task lives in the URL, not in component state.
  //
  // `Copy link` hands out `/board?task=NOOK-42`, and an agent reporting "filed
  // NOOK-42" gives a human the same thing. Both were dead: nothing read the
  // parameter, so the link opened the board with no task showing. Keeping it in
  // the query string also makes Back close the modal, which is what every
  // browser user already expects.
  const [params, setParams] = useSearchParams();
  // `null` = closed; a string preselects the type, so one modal serves both the
  // generic entry point and "New epic".
  const [newTicketType, setNewTicketType] = useState<string | null>(null);
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
  const setSelection = useBacklogSelection((s) => s.setSelection);
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
    // No timer (MAIN-365). `task_changed` already invalidates the `["boards"]`
    // prefix the moment anything moves, so the five-second poll was a second
    // mechanism racing the first — and on reconnect it joined the blanket
    // `invalidateQueries()` in a herd of refetches that blanked every panel.
    //
    // Focus is the one resync a dumb frontend still wants: column adds and
    // renames are the changes that carry no event, they are rare and
    // deliberate, and coming back to the tab is exactly when a stale column
    // would be noticed. It costs one request at the moment somebody looks,
    // instead of twelve a minute forever.
    refetchOnWindowFocus: true,
  });

  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
  // Tenant members for the specific-person assignee filter (MAIN-111). The
  // first page is plenty for a board's tenant; `principal_id` IS the user id the
  // `assignee` param filters on.
  const tenantId = me?.tenant?.id;
  const { data: members } = useQuery({
    queryKey: ["tenant-members", "board-filter", tenantId],
    enabled: !!tenantId,
    queryFn: async () =>
      (
        await api.GET("/api/v1/tenants/{id}/members", {
          params: { path: { id: tenantId as string }, query: { limit: 200 } },
        })
      ).data?.rows ?? [],
  });
  const filterMembers = React.useMemo(
    () => (members ?? []).map((m) => ({ id: m.principal_id, name: m.display_name })),
    [members],
  );
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
    filter.visibility.length > 0 ||
    filter.assignee !== "any" ||
    filter.priority !== null ||
    filter.blocked !== null ||
    filter.epic !== null ||
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
              // Type surface (MAIN-181 AC-1) — see `searchTypeParam`: on the
              // Backlog tab, no explicit type filter asks for ALL types incl.
              // `epic`, so a search/filter doesn't drop epics to the server's
              // default exclusion (MAIN-80).
              ...(searchTypeParam(filter) ? { type: searchTypeParam(filter) } : {}),
              // Server-driven, no client re-filter: the visibility param NARROWS
              // within what the viewer may already see (MAIN-103).
              ...(filter.visibility.length ? { visibility: filter.visibility } : {}),
              ...(filter.assignee === "none"
                ? { assignee: "none" }
                : filter.assignee === "me"
                  ? me?.user?.id
                    ? { assignee: me.user.id }
                    : {}
                  : filter.assignee !== "any"
                    ? { assignee: filter.assignee } // a specific person's user uuid
                    : {}),
              // Epic filter → the server `parent` query, but only on the kanban
              // tab; the Backlog tab narrows to the epic's SECTION client-side
              // (it needs the epic row too, which `parent` would exclude).
              ...(filter.epic && filter.view !== "backlog"
                ? { parent: filter.epic }
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
  // Grouping survives search (MAIN-181 AC-2): a matching CHILD must render under
  // its epic's header even when the epic itself did not match. When filtering,
  // pull in the epic rows referenced by visible children so `groupByEpic` makes
  // their sections; without it the child (an epic-parented non-epic) is orphaned
  // and vanishes. Not needed unfiltered — every epic is already visible.
  const shownEpicHeaders = filterActive ? matchedEpicHeaders(visible, detail.tasks) : [];
  const allBacklogGroups = groupByEpic(
    [...visible, ...archivedEpicChildren, ...shownEpicHeaders],
    colTypeById,
  );
  // Epic filter on the Backlog tab: show ONLY that epic's section (AC-3). The
  // kanban tab narrows server-side via the `parent` query param instead.
  const backlogGroups = filter.epic
    ? {
        epics: allBacklogGroups.epics.filter((s) => s.epic.id === filter.epic),
        noEpic: [],
      }
    : allBacklogGroups;
  const epics = detail.tasks.filter((t) => t.type === "epic");

  // Build a task's action-menu items for the shared context-menu primitive
  // (MAIN-168). The loop item's disabled state reads the task's jobs from the
  // query cache; a prefetch warms it so a repeat open is accurate (a cold first
  // open shows the action enabled — the backend stays authoritative).
  const buildItems = (task: TaskItem): ContextMenuItem[] => {
    void queryClient.prefetchQuery({
      queryKey: taskJobsKey(task.id),
      queryFn: () => fetchTaskJobs(task.id),
    });
    return taskMenuItems({
      task,
      columns: detail.columns,
      epics,
      jobs: queryClient.getQueryData<LoopJob[]>(taskJobsKey(task.id)),
      onOpen: () => setOpenTask(task.key ?? task.id),
      // MAIN-233: the spec/decompose affordance lands in the Loop workspace,
      // where the run is started with an idea instead of blind.
      onOpenLoop: (t) => navigate(`/loop/${t.key ?? t.id}`),
      onStartWork: (t) =>
        showNewWork({
          taskId: t.id,
          workspaceId: t.workspace_id ?? undefined,
          worktree: true,
        }),
      refresh: bust,
    });
  };

  // The exact-key search hit (MAIN-181 AC-3): a case-insensitive FULL-key match,
  // so typing `MAIN-34` / `main-34` highlights and scrolls to that ticket on
  // whichever tab renders it — regardless of type, column, or collapse. `null`
  // when the query is not a whole key.
  const exactKeyHitId = exactKeyMatch(detail.tasks, filter.q);

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
  // One bulk action over the current selection (MAIN-154). One call, one action;
  // the server returns a per-id result. On FULL success (nothing skipped) clear
  // the selection; on a PARTIAL keep exactly the skipped rows selected so the
  // user can see/retry them (AC-4). The board refreshes through the SAME
  // `bust()` every single-task mutation already uses — the server also publishes
  // a `TaskChanged` per task, but busting `["boards"]` is the board's own refresh
  // path. Returns the one-line summary for the toolbar to show, or null on a
  // no-op / failure.
  const applyBulk = async (action: string, value?: string): Promise<string | null> => {
    const ids = [...selected];
    if (ids.length === 0) return null;
    const { data } = await api.POST("/api/v1/tasks/bulk", {
      body: { task_ids: ids, action, value },
    });
    bust();
    if (!data) return null;
    const { skippedIds, message } = summarizeBulk(data);
    if (data.skipped === 0) clearSelection();
    else setSelection(skippedIds);
    return message;
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
            {/* The board-level entry point (MAIN-364). The per-column composer
                stays for jotting a title where it belongs; this is the one that
                takes an idea to a drafted ticket without opening one first. */}
            <span className="board-tabs-actions">
              <button
                className="btn small primary"
                onClick={() => setNewTicketType("task")}
                title="hand an agent a prompt; it writes the ticket"
              >
                <Sparkles size={11} /> Draft with AI
              </button>
              <button
                className="btn small"
                onClick={() => setNewTicketType("epic")}
                title="hand an agent a prompt; it decomposes the epic"
              >
                <Layers size={11} /> New epic
              </button>
            </span>
          </div>
          <Filters
            labels={labels ?? []}
            workspaces={workspaces ?? []}
            members={filterMembers}
            epics={epicOptions(detail.tasks, "").map((e) => ({
              id: e.id,
              key: e.key ?? "",
            }))}
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
              hitId={exactKeyHitId}
              searching={filterActive}
              selected={selected}
              blockedIds={blockedIds}
              canSendToBoard={!!unstartedColumn}
              onAddEpic={addEpic}
              onAddChild={addChild}
              onAddBacklog={(title) => backlogColumn && addTask(backlogColumn.id, title)}
              onOpen={setOpenTask}
              onMenu={(t, anchor) => openAt(anchor.x, anchor.y, buildItems(t))}
              onToggleSelect={toggleSelect}
              onSendToBoard={sendToBoard}
              onDispatch={dispatchTask}
              columns={detail.columns}
              members={filterMembers}
              onBulk={applyBulk}
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
                      menuItems={buildItems}
                      onArchiveCompleted={() => archiveCompleted(c.id)}
                      selectedId={openTask}
                      hitId={exactKeyHitId}
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
      {newTicketType && (
        <NewTicketModal
          boardId={board.id}
          initialType={newTicketType}
          onClose={() => setNewTicketType(null)}
          onCreated={({ taskId }) => {
            setNewTicketType(null);
            bust();
            // Straight to the loop: that is where the agent's questions arrive
            // and where the person answers them. The card is a placeholder the
            // run is about to rewrite, so it is not the useful place to land.
            navigate(`/loop/${taskId}`);
          }}
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
            if (t) openAt(anchor.x, anchor.y, buildItems(t));
          }}
        />
      )}
    </div>
  );
}
