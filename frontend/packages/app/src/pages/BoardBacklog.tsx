// The Backlog tab of the Board page (MAIN-82/83): a dense, prioritized list of
// the refinement queue — grouped by epic (MAIN-83). One collapsible section per
// epic, each with a done/total progress count and all its children; a final "No
// epic" section holds parentless backlog tasks. Fed by the same board_detail
// fetch as the kanban tab; no new endpoint. Rows reuse the card helpers.
import React, { useEffect, useRef, useState } from "react";
import { ArrowRight, ChevronDown, ChevronRight, MoreHorizontal, Plus, Rocket } from "lucide-react";
import { type BoardColumn, type TaskItem } from "@nookos/api";
import { TypeBadge } from "@nookos/ui";

import { priorityMeta, previewText, PRIORITIES } from "../taskmeta";
import type { BacklogGroups, EpicSection } from "./Board";
import { nextCollapsed, selectableRowIds, useBacklogSelection } from "./backlogSelection";

// The task types a bulk "set type" can pick — epic is deliberately absent: an
// epic is a container, not a workable type, and the backend rejects it (MAIN-154).
const BULK_TYPES = ["task", "bug", "story", "chore"] as const;

const COLLAPSE_KEY = "nook.backlog.collapsed";

/** A stable empty set — the "all expanded" collapse state used while searching,
 *  so a hit never hides in a collapsed epic without mutating the stored collapse
 *  (MAIN-181 AC-3; the toggle behaviour itself is untouched — NG-2). */
const NONE_COLLAPSED: Set<string> = new Set();

/** Scroll an element into view once when `when` flips true — used to bring the
 *  exact-key search hit into view (MAIN-181 AC-3). */
function useScrollToWhen<T extends HTMLElement>(when: boolean) {
  const ref = useRef<T>(null);
  useEffect(() => {
    if (when) ref.current?.scrollIntoView({ block: "center", behavior: "smooth" });
  }, [when]);
  return ref;
}

/// Collapse state that survives a reload (MAIN-83 AC-1), keyed by epic id in
/// localStorage. Returns the set of collapsed ids and a toggle. The toggle rule
/// is the pure `nextCollapsed` helper so collapse is one definition — the
/// chevron icon/title and the body render both read the SAME set, so they can
/// never desync (MAIN-123 AC-4).
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
      const next = nextCollapsed(prev, id);
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
  active,
  hit = false,
  selectable,
  selected,
  blocked,
  readOnly,
  onOpen,
  onMenu,
  onToggleSelect,
  onSendToBoard,
  onDispatch,
}: {
  task: TaskItem;
  workspaceName?: string;
  /** Status chip (column type or `archived`); omitted for the flat "No epic" rows. */
  status?: string;
  /** This row's task is the currently-OPEN card — drives the open-highlight.
   *  Distinct from `selected`, which is the bulk-selection checkbox state. */
  active: boolean;
  /** This row is the exact-key search hit — highlighted and scrolled into view
   *  (MAIN-181 AC-3). */
  hit?: boolean;
  /** Whether this row carries a selection checkbox (MAIN-123 AC-1). */
  selectable: boolean;
  /** Whether this row is checked in the bulk selection. */
  selected: boolean;
  blocked: boolean;
  /** An on-board child: opens the detail, no inline actions (MAIN-83 AC-2). */
  readOnly?: boolean;
  onOpen: () => void;
  onMenu: (anchor: { x: number; y: number }) => void;
  onToggleSelect: () => void;
  onSendToBoard?: () => void;
  onDispatch?: () => void;
}) {
  const isEpic = task.type === "epic";
  const prio = priorityMeta(task.priority ?? 0);
  const hitRef = useScrollToWhen<HTMLDivElement>(hit);
  return (
    <div
      ref={hitRef}
      className={`backlog-row${active ? " active" : ""}${hit ? " hit" : ""}${selected ? " selected" : ""}${blocked ? " blocked" : ""}${task.archived_at ? " archived" : ""}`}
      onClick={onOpen}
    >
      {selectable && (
        <input
          type="checkbox"
          className="backlog-row-check"
          checked={selected}
          // stopPropagation so ticking the box neither opens the card (the row's
          // onClick) nor lets the click bubble anywhere else (MAIN-123 AC-1).
          onClick={(e) => e.stopPropagation()}
          onChange={(e) => {
            e.stopPropagation();
            onToggleSelect();
          }}
          title="select"
        />
      )}
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
  activeId,
  hitId,
  selected,
  blockedIds,
  canSendToBoard,
  collapsed,
  onToggle,
  onOpen,
  onMenu,
  onToggleSelect,
  onSendToBoard,
  onDispatch,
  onAddChild,
}: {
  section: EpicSection;
  colTypeById: Map<string, string | undefined>;
  wsName: Map<string, string>;
  /** The currently-open task id/key — drives the open-highlight, not selection. */
  activeId: string | null;
  /** The exact-key search hit's task id (AC-3) — an epic head or a child row. */
  hitId: string | null;
  /** The bulk-selection set (task ids). */
  selected: Set<string>;
  blockedIds: Set<string>;
  canSendToBoard: boolean;
  collapsed: boolean;
  onToggle: () => void;
  onOpen: (key: string) => void;
  onMenu: (task: TaskItem, anchor: { x: number; y: number }) => void;
  onToggleSelect: (taskId: string) => void;
  onSendToBoard: (taskId: string) => void;
  onDispatch: (taskId: string) => void;
  onAddChild: (epicId: string, title: string) => void;
}) {
  const { epic, children, done, total } = section;
  const epicHit = hitId === epic.id;
  const hitRef = useScrollToWhen<HTMLDivElement>(epicHit);
  return (
    <div className={`backlog-epic${epicHit ? " hit" : ""}`}>
      {/* Clicking the head opens the epic's detail like any other row; the
         chevron is its own button so collapsing the group stays separate from
         opening it (MAIN-83 — the head used to only toggle, so an epic could
         never be opened). */}
      <div
        ref={hitRef}
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
                  active={activeId === t.key || activeId === t.id}
                  hit={hitId === t.id}
                  selectable
                  selected={selected.has(t.id)}
                  blocked={blockedIds.has(t.id)}
                  readOnly={onBoard}
                  onOpen={() => onOpen(t.key ?? t.id)}
                  onMenu={(anchor) => onMenu(t, anchor)}
                  onToggleSelect={() => onToggleSelect(t.id)}
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
  activeId,
  hitId = null,
  searching = false,
  selected,
  blockedIds,
  canSendToBoard,
  onAddEpic,
  onAddChild,
  onAddBacklog,
  onOpen,
  onMenu,
  onToggleSelect,
  onSendToBoard,
  onDispatch,
  columns,
  members,
  onBulk,
}: {
  groups: BacklogGroups;
  colTypeById: Map<string, string | undefined>;
  wsName: Map<string, string>;
  /** The currently-open task id/key — the open-highlight, NOT the selection. */
  activeId: string | null;
  /** The exact-key search hit's task id — highlighted + scrolled to (AC-3). */
  hitId?: string | null;
  /** A backlog filter/search is active: render every epic section EXPANDED so a
   *  matching child or an exact-key hit never hides in a collapsed epic (AC-2/
   *  AC-3). The stored collapse state is left untouched (NG-2). */
  searching?: boolean;
  /** The bulk-selection set (task ids), sourced from the selection store. */
  selected: Set<string>;
  blockedIds: Set<string>;
  canSendToBoard: boolean;
  onAddEpic: (title: string) => void;
  onAddChild: (epicId: string, title: string) => void;
  onAddBacklog: (title: string) => void;
  onOpen: (key: string) => void;
  onMenu: (task: TaskItem, anchor: { x: number; y: number }) => void;
  onToggleSelect: (taskId: string) => void;
  onSendToBoard: (taskId: string) => void;
  onDispatch: (taskId: string) => void;
  /** The board's columns — the "move to column" target list (value = `type`). */
  columns: BoardColumn[];
  /** Tenant members for the assignee dropdown (id = the user uuid to assign). */
  members: { id: string; name: string }[];
  /** Apply one bulk action over the current selection; resolves to the one-line
   *  summary to show (or null on a no-op / failure). */
  onBulk: (action: string, value?: string) => Promise<string | null>;
}) {
  const [collapsed, toggle] = useCollapsed();
  // While filtering/searching, treat every section as expanded so a match never
  // hides in a collapsed epic (AC-2/AC-3) — the stored collapse is untouched, so
  // clearing the search restores it exactly (AC-4, NG-2).
  const effectiveCollapsed = searching ? NONE_COLLAPSED : collapsed;
  // Select-all and clear come straight from the store: they replace the whole
  // set at once, so there's no per-row callback to thread for them.
  const setSelection = useBacklogSelection((s) => s.setSelection);
  const clear = useBacklogSelection((s) => s.clear);
  const empty = groups.epics.length === 0 && groups.noEpic.length === 0;

  // The last bulk call's one-line summary, and a busy flag so the controls lock
  // while a call is in flight (a second click would act on a selection the first
  // call is about to change out from under it). The summary lives here — not in
  // the store — so it survives the selection being cleared on full success.
  const [summary, setSummary] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const runBulk = async (action: string, value?: string) => {
    if (busy) return;
    setBusy(true);
    try {
      const msg = await onBulk(action, value);
      if (msg !== null) setSummary(msg);
    } finally {
      setBusy(false);
    }
  };

  // Every currently-VISIBLE selectable row — respects filters (already applied
  // to `groups`) and collapsed epics (their hidden children drop out). This is
  // exactly what select-all targets and what the "all selected?" check reads.
  const visibleIds = selectableRowIds(groups, effectiveCollapsed);
  const allSelected =
    visibleIds.length > 0 && visibleIds.every((id) => selected.has(id));
  const toggleAll = () => {
    if (allSelected) clear();
    else setSelection(visibleIds);
  };

  return (
    <div className="backlog">
      <BacklogComposer onAdd={onAddEpic} placeholder="New epic…" label="epic" />

      {/* Select-all + the bulk-action toolbar (MAIN-123 AC-2/AC-3, MAIN-154).
         With ≥1 row selected it carries the six bulk actions: agent-ready
         on/off, move-to-column, priority, type, assignee, archive. Each acts on
         the whole selection in one call; the selects are "action" selects — a
         constant value="" so they snap back to their placeholder after firing. */}
      <div className="backlog-select-toolbar">
        <label className="backlog-select-all" title="select all visible rows">
          <input
            type="checkbox"
            className="backlog-row-check"
            checked={allSelected}
            disabled={visibleIds.length === 0}
            onChange={toggleAll}
          />
          all
        </label>
        {selected.size >= 1 && (
          <div className="backlog-select-actions">
            <span className="backlog-select-count">{selected.size} selected</span>

            <span className="backlog-bulk-group" title="toggle the agent-ready label">
              <span className="faint">agent-ready</span>
              <button
                className="btn small"
                disabled={busy}
                onClick={() => runBulk("agent_ready", "on")}
              >
                on
              </button>
              <button
                className="btn small"
                disabled={busy}
                onClick={() => runBulk("agent_ready", "off")}
              >
                off
              </button>
            </span>

            <select
              className="task-select"
              value=""
              disabled={busy}
              title="move to column"
              onChange={(e) => e.target.value && runBulk("move_column", e.target.value)}
            >
              <option value="">move to…</option>
              {columns.map((c) => (
                <option key={c.id} value={c.type}>
                  {c.name}
                </option>
              ))}
            </select>

            <select
              className="task-select"
              value=""
              disabled={busy}
              title="set priority"
              onChange={(e) => e.target.value !== "" && runBulk("priority", e.target.value)}
            >
              <option value="">priority…</option>
              {PRIORITIES.map((p) => (
                <option key={p.value} value={String(p.value)}>
                  {p.label}
                </option>
              ))}
            </select>

            <select
              className="task-select"
              value=""
              disabled={busy}
              title="set type"
              onChange={(e) => e.target.value && runBulk("type", e.target.value)}
            >
              <option value="">type…</option>
              {BULK_TYPES.map((t) => (
                <option key={t} value={t}>
                  {t}
                </option>
              ))}
            </select>

            <select
              className="task-select"
              value=""
              disabled={busy}
              title="set assignee"
              onChange={(e) => {
                const v = e.target.value;
                if (!v) return;
                // "Unassign" sends an empty value; the backend reads that as clear.
                runBulk("assignee", v === "__unassign__" ? "" : v);
              }}
            >
              <option value="">assign…</option>
              <option value="__unassign__">Unassign</option>
              {members.map((m) => (
                <option key={m.id} value={m.id}>
                  {m.name}
                </option>
              ))}
            </select>

            <button
              className="btn small"
              disabled={busy}
              onClick={() => runBulk("archive")}
              title="archive selected"
            >
              archive
            </button>

            <button
              className="btn small"
              onClick={() => {
                clear();
                setSummary(null);
              }}
              title="clear selection"
            >
              clear
            </button>
          </div>
        )}
        {summary && <span className="backlog-select-summary faint">{summary}</span>}
      </div>

      {empty && <div className="board-no-matches faint small">The backlog is empty.</div>}

      {groups.epics.map((section) => (
        <EpicGroup
          key={section.epic.id}
          section={section}
          colTypeById={colTypeById}
          wsName={wsName}
          activeId={activeId}
          hitId={hitId}
          selected={selected}
          blockedIds={blockedIds}
          canSendToBoard={canSendToBoard}
          collapsed={effectiveCollapsed.has(section.epic.id)}
          onToggle={() => toggle(section.epic.id)}
          onOpen={onOpen}
          onMenu={onMenu}
          onToggleSelect={onToggleSelect}
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
              active={activeId === t.key || activeId === t.id}
              hit={hitId === t.id}
              selectable
              selected={selected.has(t.id)}
              blocked={blockedIds.has(t.id)}
              onOpen={() => onOpen(t.key ?? t.id)}
              onMenu={(anchor) => onMenu(t, anchor)}
              onToggleSelect={() => onToggleSelect(t.id)}
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
