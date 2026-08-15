// The dashboard's Queue panel (MAIN-451).
//
// The dashboard used to give its biggest slot to an activity feed — a log of
// what already happened — when the question people arrive with is what is about
// to happen and what is stuck. This answers that: the exact list a builder would
// pick from, what is being worked, what is waiting on review, and what was
// handed back for a human.
//
// **Read-only, deliberately (NG-4).** Nothing here claims, moves, labels or
// dispatches. It is a window on the board, and every row is a link to the card
// on the Board where those actions live.
import React from "react";
import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import { api, type TaskItem } from "@nookos/api";
import { Empty, Panel } from "@nookos/ui";

import { priorityMeta } from "./taskmeta";
import { WorkspacePicker } from "./WorkspacePicker";
import { useWorkspaceNames } from "./workspaces";

/// How many rows each section shows before it stops and offers the Board.
///
/// **On deck is the long one on purpose.** It is the section people read as a
/// list — "what comes after this one" — where the others are read as counts,
/// and ten is about where a glance stops being a glance.
const ON_DECK_CAP = 10;
const SECTION_CAP = 5;

/// A claim nobody has touched for this long is worth looking at.
///
/// Two hours sits deliberately below `max_claim_secs` (4h by default), which is
/// where the claim reaper escalates: this is the "somebody should look" mark,
/// arriving before the "the fleet gave up" one.
export const STALE_CLAIM_MS = 2 * 60 * 60 * 1000;

/// The server's own page size, asked for explicitly so the `+N more` count is a
/// real number rather than "at least the cap". Fifty is what `/tasks` returns
/// unasked, so this changes no request the server has not always served.
const FETCH = 50;

/// How long a claimed card has gone untouched, in milliseconds.
///
/// **This is `updated_at`, and the distinction matters.** There is no
/// `claimed_at` column — a claim writes `claim_expires_at` and stamps
/// `updated_at`, and nothing in the tree renews a lease (the renew endpoint has
/// no caller), so for a claimed card `updated_at` IS when it was claimed unless
/// something has touched it since. Adding a column would be a backend change,
/// which this card forbids (NG-1).
///
/// So the number is "time since last activity on a claimed card", which is the
/// signal AC-5 exists for: a worker that crashed stops touching its card, and
/// the row goes quiet. A card somebody commented on ten minutes ago reads as
/// ten minutes — correctly, because somebody is there.
///
/// `null` for a card with no lease at all: a human dragged it into In Progress,
/// nothing claimed it, and labelling that with a claim age would be a fiction.
export function claimAgeMs(task: TaskItem, now: number): number | null {
  if (!task.claim_expires_at) return null;
  const updated = Date.parse(task.updated_at ?? "");
  if (Number.isNaN(updated)) return null;
  return Math.max(0, now - updated);
}

/// A duration in the shortest form that still says which unit it is.
export function shortAge(ms: number): string {
  const mins = Math.floor(ms / 60000);
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins}m`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return mins % 60 ? `${hours}h ${mins % 60}m` : `${hours}h`;
  return `${Math.floor(hours / 24)}d`;
}

/// Merge label queries into one list, keeping the first appearance of each card.
///
/// Two queries rather than one, because repeated `label=` parameters AND on the
/// server ("Repeatable. ALL must be present") — a single request for both labels
/// would return only cards carrying both, which is almost never what anyone
/// means. Deduping here is the cost of not changing that (NG-1).
export function mergeById(...lists: (TaskItem[] | undefined)[]): TaskItem[] {
  const seen = new Set<string>();
  const out: TaskItem[] = [];
  for (const list of lists) {
    for (const t of list ?? []) {
      if (seen.has(t.id)) continue;
      seen.add(t.id);
      out.push(t);
    }
  }
  return out;
}

/// Where the workspace filter is remembered. Per browser, not per user: it is a
/// view preference, not something the server has an opinion about (NG-7).
const FILTER_KEY = "nook.queue.workspace";

function storedWorkspace(): string {
  try {
    return localStorage.getItem(FILTER_KEY) ?? "";
  } catch {
    // A browser with storage disabled still gets a working panel, it just
    // forgets the filter — which is better than a dashboard that throws.
    return "";
  }
}

type Query = Record<string, string | number | boolean | string[]>;

/// One section's query, with the workspace filter folded in.
///
/// The filter narrows EVERY section (AC-6), so it is applied here rather than at
/// four call sites — one of which would eventually be forgotten, and the symptom
/// would be a panel that half-narrows.
function scoped(base: Query, workspace: string): Query {
  return workspace ? { ...base, workspace } : base;
}

function useTasks(key: string, query: Query, enabled = true) {
  return useQuery({
    // The `["tasks", …]` prefix is what makes this live with no polling: the
    // websocket's `task_changed` invalidates that whole prefix (AC-7).
    queryKey: ["tasks", "queue", key, query],
    queryFn: async () =>
      (await api.GET("/api/v1/tasks", { params: { query: query as never } })).data ?? [],
    enabled,
  });
}

export function QueuePanel() {
  const [workspace, setWorkspace] = React.useState(storedWorkspace);
  // Recomputed per render rather than ticked on a timer: every section already
  // re-renders when the board changes, and a claim age that is sixty seconds
  // stale is not a number anybody is reading that closely.
  const now = Date.now();

  const setFilter = (id: string) => {
    setWorkspace(id);
    try {
      if (id) localStorage.setItem(FILTER_KEY, id);
      else localStorage.removeItem(FILTER_KEY);
    } catch {
      // Storage refused; the filter still applies for this page view.
    }
  };

  // EXACTLY the builder's pick (AC-2). The same four filters `nook tasks` sends,
  // in the same order the server returns them — backlog and epics are already
  // excluded server-side, so this list is what the next agent takes, not an
  // approximation of it.
  const onDeck = useTasks(
    "on-deck",
    scoped(
      {
        label: ["agent-ready"],
        not_label: ["blocked"],
        assignee: "none",
        is_blocked: false,
        limit: FETCH,
      },
      workspace,
    ),
  );
  const started = useTasks(
    "started",
    scoped({ column_type: "started", limit: FETCH }, workspace),
  );
  const review = useTasks(
    "review",
    scoped({ column_type: "review", limit: FETCH }, workspace),
  );
  const blocked = useTasks(
    "blocked",
    scoped({ label: ["blocked"], limit: FETCH }, workspace),
  );
  const needsHuman = useTasks(
    "human",
    scoped({ label: ["human-review-required"], limit: FETCH }, workspace),
  );

  // A card names its workspace, so the badge reads THAT row (MAIN-606) rather
  // than indexing a collection the panel can no longer hold whole.
  const names = useWorkspaceNames([
    workspace,
    ...[onDeck, started, review, blocked, needsHuman].flatMap((q) =>
      (q.data ?? []).map((t) => t.workspace_id),
    ),
  ]);
  const wsName = (id: string | null | undefined) => (id && names.get(id)) || "";

  return (
    <Panel
      title="Queue"
      style={{ gridRow: "1 / span 2" }}
      actions={
        <WorkspacePicker
          value={workspace}
          onChange={setFilter}
          noneLabel="All workspaces"
          ariaLabel="workspace filter"
        />
      }
    >
      <div className="queue-panel">
        <Section
          title="On deck"
          tasks={onDeck.data}
          loading={onDeck.isPending}
          cap={ON_DECK_CAP}
          numbered
          workspace={workspace}
          wsName={wsName}
          // Not an error, and worth saying so: a board with nothing approved is
          // the normal state of a board somebody has just cleared.
          empty="Nothing approved and waiting — no agent-ready work in the queue."
        />
        <Section
          title="In progress"
          tasks={started.data}
          loading={started.isPending}
          cap={SECTION_CAP}
          workspace={workspace}
          wsName={wsName}
          age={(t) => claimAgeMs(t, now)}
          empty="Nobody is working anything right now."
        />
        <Section
          title="In review"
          tasks={review.data}
          loading={review.isPending}
          cap={SECTION_CAP}
          workspace={workspace}
          wsName={wsName}
          empty="No PRs waiting on a review."
        />
        <Section
          title="Blocked / needs human"
          tasks={mergeById(blocked.data, needsHuman.data)}
          loading={blocked.isPending || needsHuman.isPending}
          cap={SECTION_CAP}
          workspace={workspace}
          wsName={wsName}
          // The one empty state that is good news, and it should read that way.
          empty="Nothing is waiting on a human."
        />
      </div>
    </Panel>
  );
}

function Section({
  title,
  tasks,
  loading,
  cap,
  numbered = false,
  workspace,
  wsName,
  age,
  empty,
}: {
  title: string;
  tasks: TaskItem[] | undefined;
  loading: boolean;
  cap: number;
  numbered?: boolean;
  workspace: string;
  wsName: (id: string | null | undefined) => string;
  age?: (t: TaskItem) => number | null;
  empty: string;
}) {
  const all = tasks ?? [];
  const shown = all.slice(0, cap);
  const more = all.length - shown.length;

  return (
    <section className="queue-section">
      <div className="queue-section-head">
        <span className="bright">{title}</span>
        <span className="faint mono small">{loading ? "" : all.length}</span>
      </div>
      {loading ? (
        // Distinct from every empty state below (AC-8): "we have not asked yet"
        // and "we asked and there is none" are different facts, and a section
        // that showed its empty message while loading would read as a board
        // that had emptied itself.
        <div className="empty">Loading…</div>
      ) : shown.length === 0 ? (
        <Empty>{empty}</Empty>
      ) : (
        <table className="nook-table queue-table">
          <tbody>
            {shown.map((t, i) => (
              <Row
                key={t.id}
                task={t}
                index={numbered ? i + 1 : null}
                wsName={wsName}
                ageMs={age?.(t) ?? null}
              />
            ))}
          </tbody>
        </table>
      )}
      {more > 0 && (
        <Link
          to={workspace ? `/board?workspace=${workspace}` : "/board"}
          className="queue-more faint small"
        >
          +{more} more
        </Link>
      )}
    </section>
  );
}

function Row({
  task,
  index,
  wsName,
  ageMs,
}: {
  task: TaskItem;
  /** Position in the pick order, or `null` for a section that is not a queue. */
  index: number | null;
  wsName: (id: string | null | undefined) => string;
  ageMs: number | null;
}) {
  const priority = priorityMeta(task.priority);
  const stale = ageMs !== null && ageMs >= STALE_CLAIM_MS;
  const ws = wsName(task.workspace_id);

  return (
    <tr>
      {index !== null && (
        <td className="mono faint queue-index">{String(index).padStart(2, "0")}</td>
      )}
      <td className="mono queue-key">
        <Link to={`/board?task=${task.key}`} className="bright">
          {task.key}
        </Link>
      </td>
      <td className="queue-title">
        <Link to={`/board?task=${task.key}`}>{task.title}</Link>
      </td>
      <td
        className="mono small queue-priority"
        style={{ color: priority.color }}
        // The ordering IS the priority (AC-4), so the glyph carries the word
        // rather than spending a column on it.
        title={`priority: ${priority.label}`}
      >
        {priority.mark}
      </td>
      <td className="muted small queue-workspace">{ws}</td>
      {ageMs !== null && (
        <td
          className={`mono small queue-age${stale ? " err" : ""}`}
          title={
            stale
              ? "Claimed and untouched for over two hours — the worker may be gone."
              : "Time since this claimed card was last touched."
          }
        >
          {shortAge(ageMs)}
        </td>
      )}
    </tr>
  );
}
