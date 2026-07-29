import React, { useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "react-router-dom";
import {
  ChevronDown,
  ChevronRight,
  CircleDot,
  Eye,
  EyeOff,
  FolderGit2,
  GitBranch,
  Loader2,
  MoreHorizontal,
  SquareTerminal,
} from "lucide-react";
import { api } from "@nookos/api";
import type { OverviewCheckout, OverviewWorkspace, Session } from "@nookos/api";
import { Empty, Panel, Pill, StatusDot, statusTone } from "@nookos/ui";
import { useNewWork } from "../newwork";
import { notify } from "../dialogs";
import { liveAgentMark, useLive, type AgentState } from "../live";
import {
  ContextMenuRegion,
  useContextMenuApi,
  type ContextMenuItem,
} from "../contextMenu";
import {
  canAddWorktree,
  canOpenTerminal,
  deckStats,
  exceptionCounts,
  groupCheckoutsByNode,
  isMissing,
  loadCollapsed,
  loadView,
  machineGroups,
  matrixData,
  overlayLive,
  repoRollup,
  saveCollapsed,
  saveView,
  visibleRepos,
  type Lamp,
  type MissionView,
  type VisibleRepo,
} from "../mission";

/** Everything a checkout row can do, built once and served two ways: the row's
 *  ⋯ button and its right-click menu are the same list. */
interface RowActions {
  onTerminal: (c: OverviewCheckout) => void;
  onWorktree: (c: OverviewCheckout) => void;
  openWorkspace: (id: string) => void;
}

function checkoutMenuItems(
  c: OverviewCheckout,
  ws: OverviewWorkspace,
  act: RowActions,
): ContextMenuItem[] {
  const items: ContextMenuItem[] = [];
  if (canOpenTerminal(c)) {
    items.push({
      label: "Terminal here",
      icon: <SquareTerminal size={13} />,
      onSelect: () => act.onTerminal(c),
    });
  }
  if (canAddWorktree(c)) {
    items.push({
      label: "New worktree from this clone",
      icon: <GitBranch size={13} />,
      onSelect: () => act.onWorktree(c),
    });
  }
  if (items.length > 0) items.push({ separator: true });
  items.push({
    label: "Copy path",
    onSelect: () => {
      navigator.clipboard?.writeText(c.path).catch(() => {});
    },
  });
  items.push({
    label: "Open workspace",
    onSelect: () => act.openWorkspace(ws.id),
  });
  return items;
}

/** The ⋯ button: opens the shared context menu anchored under itself, so the
 *  click path and the right-click path land in the identical menu. */
function RowMenuButton({
  id,
  items,
}: {
  id: string;
  items: () => ContextMenuItem[];
}) {
  const menu = useContextMenuApi();
  return (
    <button
      className="btn small icon m-row-menu"
      data-testid={`rowmenu-${id}`}
      title="actions"
      aria-haspopup="menu"
      onClick={(e) => {
        e.stopPropagation();
        const r = e.currentTarget.getBoundingClientRect();
        menu.openAt(r.left, r.bottom + 2, items());
      }}
    >
      <MoreHorizontal size={13} />
    </button>
  );
}

/// Mission Control (MAIN-226): every repo × machine × checkout × session on one
/// dense screen. An annunciator deck on top — fleet counters, live agent chips,
/// exception lamps that double as filters — and below it the fleet in the
/// flavor of your choice: tree, grid, machines, or the repos × machines matrix.
export function MissionPage() {
  const showNewWork = useNewWork((s) => s.show);
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [q, setQ] = useState("");
  const [lamp, setLamp] = useState<Lamp | null>(null);
  const [ghosts, setGhosts] = useState(false);
  const [view, setView] = useState<MissionView>(() => loadView());
  const [collapsed, setCollapsed] = useState<Set<string>>(() =>
    loadCollapsed(),
  );

  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
  const { data: overview } = useQuery({
    queryKey: ["overview"],
    queryFn: async () => (await api.GET("/api/v1/overview")).data,
  });

  // The websocket deltas laid over the REST payload, so status changes land
  // without a refetch racing them.
  const nodeStatus = useLive((s) => s.nodeStatus);
  const sessionStatus = useLive((s) => s.sessionStatus);
  const agentState = useLive((s) => s.agentState);
  const live = useMemo(
    () => overlayLive(overview, nodeStatus, sessionStatus),
    [overview, nodeStatus, sessionStatus],
  );

  const stats = deckStats(live);
  const counts = exceptionCounts(live);
  const repos = visibleRepos(live, q, lamp, ghosts);
  // Filtering overrides collapse: a match hidden inside a collapsed repo would
  // read as "no match".
  const filtering = q.trim() !== "" || lamp !== null;

  // Live agent chips: every session whose agent is running or waiting, wherever
  // it sits in the tree. Waiting first — those are waiting on YOU.
  const chips = useMemo(() => {
    if (!live) return [];
    const out: { id: string; name: string; state: string }[] = [];
    const consider = (s: Session) => {
      const mark = liveAgentMark(s.status, agentState[s.id]);
      if (mark) out.push({ id: s.id, name: s.name, state: mark.state });
    };
    for (const w of live.workspaces) {
      for (const c of w.checkouts) c.sessions.forEach(consider);
      w.unbound_sessions.forEach(consider);
    }
    live.loose_sessions.forEach(consider);
    return out.sort((a, b) =>
      a.state === b.state ? 0 : a.state === "waiting" ? -1 : 1,
    );
  }, [live, agentState]);

  const toggleRepo = (id: string) => {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (!next.delete(id)) next.add(id);
      saveCollapsed(next);
      return next;
    });
  };

  // "Terminal here": start a bash session pinned to this exact checkout path —
  // the capability that kills the "can't open a session in an existing worktree"
  // dead-end (MAIN-222 binding). Reuses the ordinary create-session endpoint.
  const terminalHere = async (ws: OverviewWorkspace, c: OverviewCheckout) => {
    const {
      data: session,
      error,
      response,
    } = await api.POST("/api/v1/sessions", {
      body: {
        workspace_id: ws.id,
        node_id: c.node_id,
        runtime: "bash",
        path: c.path,
      },
    });
    if (error || !response.ok) {
      await notify(
        "Terminal failed",
        error
          ? String((error as { error: unknown }).error)
          : response.statusText,
      );
      return;
    }
    await queryClient.invalidateQueries({ queryKey: ["overview"] });
    if (session?.id) navigate(`/sessions/${session.id}`);
  };

  const actionsFor = (ws: OverviewWorkspace): RowActions => ({
    onTerminal: (c) => void terminalHere(ws, c),
    onWorktree: (c) =>
      showNewWork({ workspaceId: ws.id, nodeId: c.node_id, worktree: true }),
    openWorkspace: (id) => navigate(`/workspaces/${id}`),
  });

  const meId = me?.user?.id;
  const anythingAtAll =
    (live?.workspaces.length ?? 0) > 0 ||
    (live?.loose_sessions.length ?? 0) > 0;

  const lampButton = (kind: Lamp, tone: "warn" | "err", count: number) =>
    count > 0 ? (
      <button
        key={kind}
        className={`m-lamp ${tone}${lamp === kind ? " active" : ""}`}
        aria-pressed={lamp === kind}
        data-testid={`lamp-${kind}`}
        title={
          lamp === kind
            ? "clear this filter"
            : kind === "offline"
              ? "show only checkouts on offline machines"
              : `show only ${kind} checkouts`
        }
        onClick={() => setLamp(lamp === kind ? null : kind)}
      >
        {count} {kind}
      </button>
    ) : null;

  const looseSection =
    (live?.loose_sessions.length ?? 0) > 0 && !lamp ? (
      <section className="m-repo" data-testid="loose-sessions">
        <div className="m-repo-head static">
          <SquareTerminal size={13} className="m-repo-icon" />
          <span className="m-repo-name muted">ad-hoc terminals</span>
          <span className="m-repo-remote">no workspace</span>
        </div>
        <div className="m-node">
          {live!.loose_sessions.map((s) => (
            <SessionRow key={s.id} s={s} agentState={agentState} meId={meId} />
          ))}
        </div>
      </section>
    ) : null;

  return (
    <div
      className="nook-grid"
      style={{ gridTemplateColumns: "1fr", gridTemplateRows: "1fr" }}
    >
      <Panel
        className="m-panel"
        title="Mission Control"
        actions={
          <span
            style={{ display: "inline-flex", alignItems: "center", gap: 6 }}
          >
            <span className="m-views" role="tablist" aria-label="view">
              {(["tree", "grid", "machines", "matrix"] as MissionView[]).map(
                (v) => (
                  <button
                    key={v}
                    role="tab"
                    aria-selected={view === v}
                    className={`m-view-btn${view === v ? " active" : ""}`}
                    data-testid={`view-${v}`}
                    onClick={() => {
                      setView(v);
                      saveView(v);
                    }}
                  >
                    {v}
                  </button>
                ),
              )}
            </span>
            <input
              className="input small mono"
              placeholder="filter repo / node / branch / session…"
              value={q}
              onChange={(e) => setQ(e.target.value)}
              style={{ width: 210 }}
              aria-label="filter"
            />
            <button
              className={`btn small${ghosts ? " primary" : ""}`}
              aria-pressed={ghosts}
              data-testid="ghost-toggle"
              title="show checkouts that have vanished from disk"
              onClick={() => setGhosts((g) => !g)}
            >
              {ghosts ? <Eye size={12} /> : <EyeOff size={12} />} ghosts
              {counts.missing > 0 ? ` (${counts.missing})` : ""}
            </button>
          </span>
        }
      >
        <div className="m-deck" data-testid="deck">
          <span className="m-deck-stats">
            {stats.nodesOnline}/{stats.nodesTotal} node
            {stats.nodesTotal === 1 ? "" : "s"} · {stats.repos} repo
            {stats.repos === 1 ? "" : "s"} · {stats.checkouts} checkout
            {stats.checkouts === 1 ? "" : "s"} · {stats.sessions} session
            {stats.sessions === 1 ? "" : "s"}
          </span>
          {chips.length > 0 && (
            <span className="m-deck-agents">
              {chips.map((c) => (
                <button
                  key={c.id}
                  className={`m-agent ${c.state}`}
                  data-testid={`chip-${c.id}`}
                  title={
                    c.state === "waiting"
                      ? "agent is waiting on you — open the session"
                      : "agent is working — open the session"
                  }
                  onClick={() => navigate(`/sessions/${c.id}`)}
                >
                  {c.state === "running" ? (
                    <Loader2 size={11} className="spin" />
                  ) : (
                    <CircleDot size={11} />
                  )}
                  {c.name}
                </button>
              ))}
            </span>
          )}
          <span className="m-deck-lamps">
            {lampButton("dirty", "warn", counts.dirty)}
            {lampButton("missing", "warn", counts.missing)}
            {lampButton("offline", "err", counts.offline)}
          </span>
        </div>

        {!anythingAtAll ? (
          <Empty>
            Nothing running or checked out yet. Clone a repo or start work — it
            appears here grouped by machine.
          </Empty>
        ) : repos.length === 0 && (live?.loose_sessions.length ?? 0) === 0 ? (
          <Empty>
            {lamp
              ? `Nothing matches the ${lamp} filter.`
              : `Nothing matches “${q}”.`}
          </Empty>
        ) : view === "grid" ? (
          <div className="m-grid" data-testid="grid-view">
            {repos.map((r) => (
              <GridCard
                key={r.workspace.id}
                r={r}
                actions={actionsFor(r.workspace)}
                agentState={agentState}
                meId={meId}
                onShowGhosts={() => setGhosts(true)}
              />
            ))}
            {looseSection}
          </div>
        ) : view === "machines" ? (
          <div className="m-grid" data-testid="machines-view">
            {machineGroups(repos).map((g) => (
              <section
                className="m-repo"
                key={g.nodeId}
                data-testid={`machine-${g.nodeId}`}
              >
                <div className="m-repo-head static">
                  <StatusDot status={g.nodeStatus} />
                  <span className="m-repo-name bright">{g.nodeName}</span>
                  {g.nodeStatus !== "online" && <Pill tone="err">offline</Pill>}
                  <span className="m-repo-roll">
                    <span className="m-roll-dim">
                      {g.entries.length} checkout
                      {g.entries.length === 1 ? "" : "s"}
                    </span>
                  </span>
                </div>
                <div className="m-card-body">
                  {g.entries.map(({ workspace, checkout: c }) => (
                    <div key={c.id} className="m-co">
                      <CheckoutRow
                        c={c}
                        ws={workspace}
                        actions={actionsFor(workspace)}
                        prefix={
                          <Link
                            className="m-co-repo bright"
                            to={`/workspaces/${workspace.id}`}
                          >
                            {workspace.name}
                          </Link>
                        }
                        showPath={false}
                      />
                      {c.sessions.map((s) => (
                        <SessionRow
                          key={s.id}
                          s={s}
                          agentState={agentState}
                          meId={meId}
                        />
                      ))}
                    </div>
                  ))}
                </div>
              </section>
            ))}
            {looseSection}
          </div>
        ) : view === "matrix" ? (
          <MatrixView
            repos={repos}
            actionsFor={actionsFor}
            looseSection={looseSection}
          />
        ) : (
          <div className="m-tree">
            {repos.map((r) => (
              <RepoSection
                key={r.workspace.id}
                r={r}
                collapsed={!filtering && collapsed.has(r.workspace.id)}
                onToggle={() => toggleRepo(r.workspace.id)}
                actions={actionsFor(r.workspace)}
                agentState={agentState}
                meId={meId}
                onShowGhosts={() => setGhosts(true)}
              />
            ))}
            {looseSection}
          </div>
        )}
      </Panel>
    </div>
  );
}

/** One checkout line, shared by the tree / grid / machines views: kind glyph,
 *  bright branch, optional faint path, exception pill, the ⋯ menu — and the
 *  same menu on right-click. */
function CheckoutRow({
  c,
  ws,
  actions,
  prefix,
  suffix,
  showPath = true,
}: {
  c: OverviewCheckout;
  ws: OverviewWorkspace;
  actions: RowActions;
  prefix?: React.ReactNode;
  suffix?: React.ReactNode;
  showPath?: boolean;
}) {
  const items = () => checkoutMenuItems(c, ws, actions);
  return (
    <ContextMenuRegion items={items}>
      <div
        className={`m-co-row${isMissing(c) ? " ghost" : ""}`}
        data-testid={`checkout-${c.id}`}
      >
        <span className="m-co-kind" title={c.kind}>
          {c.kind === "worktree" ? (
            <GitBranch size={12} />
          ) : (
            <FolderGit2 size={12} />
          )}
        </span>
        {prefix}
        <span className="m-co-branch">{c.branch ?? "—"}</span>
        {suffix}
        {showPath && (
          <span className="m-co-path" title={c.path}>
            {c.path}
          </span>
        )}
        {isMissing(c) ? (
          <Pill tone="warn" title="this checkout has vanished from disk">
            missing
          </Pill>
        ) : c.dirty ? (
          <Pill tone="warn" title="uncommitted changes">
            dirty
          </Pill>
        ) : null}
        <span className="m-co-actions">
          <RowMenuButton id={c.id} items={items} />
        </span>
      </div>
    </ContextMenuRegion>
  );
}

function RepoSection({
  r,
  collapsed,
  onToggle,
  actions,
  agentState,
  meId,
  onShowGhosts,
}: {
  r: VisibleRepo;
  collapsed: boolean;
  onToggle: () => void;
  actions: RowActions;
  agentState: Record<string, AgentState>;
  meId?: string;
  onShowGhosts: () => void;
}) {
  const { workspace: w, checkouts, hiddenGhosts } = r;
  const nodes = groupCheckoutsByNode(checkouts);
  const roll = repoRollup(w);
  return (
    <section className="m-repo" data-testid={`repo-${w.slug}`}>
      <div
        className="m-repo-head"
        role="button"
        tabIndex={0}
        aria-expanded={!collapsed}
        onClick={onToggle}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            onToggle();
          }
        }}
      >
        {collapsed ? (
          <ChevronRight size={13} className="m-repo-icon" />
        ) : (
          <ChevronDown size={13} className="m-repo-icon" />
        )}
        <Link
          className="m-repo-name bright"
          to={`/workspaces/${w.id}`}
          onClick={(e) => e.stopPropagation()}
        >
          {w.name}
        </Link>
        <span className="m-repo-remote" title={w.git_remote_url ?? undefined}>
          {w.git_remote_normalized ?? w.git_remote_url ?? "no remote"}
        </span>
        <span className="m-repo-roll">
          {roll.sessions > 0 && (
            <span
              className="m-roll-live"
              title={`${roll.sessions} active session(s)`}
            >
              ● {roll.sessions}
            </span>
          )}
          <span className="m-roll-dim">
            {roll.checkouts} checkout{roll.checkouts === 1 ? "" : "s"}
          </span>
          {collapsed && roll.dirty > 0 && (
            <Pill tone="warn">{roll.dirty} dirty</Pill>
          )}
          {collapsed && roll.missing > 0 && (
            <Pill tone="warn">{roll.missing} missing</Pill>
          )}
        </span>
      </div>

      {!collapsed &&
        nodes.map((n) => (
          <div key={n.nodeId} className="m-node">
            <div className="m-node-head">
              <StatusDot status={n.nodeStatus} />
              <span className="m-node-name">{n.nodeName}</span>
              {n.nodeStatus !== "online" && <Pill tone="err">offline</Pill>}
            </div>
            {n.checkouts.map((c) => (
              <div key={c.id} className="m-co">
                <CheckoutRow c={c} ws={w} actions={actions} />
                {c.sessions.map((s) => (
                  <SessionRow
                    key={s.id}
                    s={s}
                    agentState={agentState}
                    meId={meId}
                  />
                ))}
              </div>
            ))}
          </div>
        ))}

      {!collapsed && hiddenGhosts > 0 && (
        <button
          className="m-ghost-hint"
          data-testid={`ghosts-${w.slug}`}
          onClick={onShowGhosts}
        >
          {hiddenGhosts} vanished checkout{hiddenGhosts === 1 ? "" : "s"} hidden
          — show ghosts
        </button>
      )}

      {!collapsed && w.unbound_sessions.length > 0 && (
        <div className="m-node" data-testid={`unbound-${w.slug}`}>
          <div className="m-node-head">
            <span className="m-node-name muted">
              sessions without a live checkout
            </span>
          </div>
          {w.unbound_sessions.map((s) => (
            <SessionRow key={s.id} s={s} agentState={agentState} meId={meId} />
          ))}
        </div>
      )}
    </section>
  );
}

/** Grid flavor: one compact card per repo; node context rides on each row as a
 *  faint suffix instead of a subhead. */
function GridCard({
  r,
  actions,
  agentState,
  meId,
  onShowGhosts,
}: {
  r: VisibleRepo;
  actions: RowActions;
  agentState: Record<string, AgentState>;
  meId?: string;
  onShowGhosts: () => void;
}) {
  const { workspace: w, checkouts, hiddenGhosts } = r;
  const roll = repoRollup(w);
  return (
    <section className="m-repo" data-testid={`card-${w.slug}`}>
      <div className="m-repo-head static">
        <FolderGit2 size={13} className="m-repo-icon" />
        <Link className="m-repo-name bright" to={`/workspaces/${w.id}`}>
          {w.name}
        </Link>
        <span className="m-repo-roll">
          {roll.sessions > 0 && (
            <span className="m-roll-live">● {roll.sessions}</span>
          )}
          <span className="m-roll-dim">{roll.checkouts} co</span>
        </span>
      </div>
      <div className="m-card-body">
        {checkouts.map((c) => (
          <div key={c.id} className="m-co">
            <CheckoutRow
              c={c}
              ws={w}
              actions={actions}
              showPath={false}
              suffix={<span className="m-co-node">@{c.node_name}</span>}
            />
            {c.sessions.map((s) => (
              <SessionRow
                key={s.id}
                s={s}
                agentState={agentState}
                meId={meId}
              />
            ))}
          </div>
        ))}
        {hiddenGhosts > 0 && (
          <button className="m-ghost-hint" onClick={onShowGhosts}>
            {hiddenGhosts} ghost{hiddenGhosts === 1 ? "" : "s"} hidden
          </button>
        )}
      </div>
    </section>
  );
}

/** Matrix flavor: repos × machines, each cell the checkouts of that repo on
 *  that machine as clickable chips (the chip opens the same actions menu). */
function MatrixView({
  repos,
  actionsFor,
  looseSection,
}: {
  repos: VisibleRepo[];
  actionsFor: (ws: OverviewWorkspace) => RowActions;
  looseSection: React.ReactNode;
}) {
  const menu = useContextMenuApi();
  const { nodes, rows } = matrixData(repos);
  return (
    <div className="m-matrix-wrap" data-testid="matrix-view">
      <table className="nook-table m-matrix">
        <thead>
          <tr>
            <th>repo</th>
            {nodes.map((n) => (
              <th key={n.id}>
                <span className="m-matrix-node">
                  <StatusDot status={n.status} /> {n.name}
                </span>
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={row.workspace.id}>
              <td>
                <Link className="bright" to={`/workspaces/${row.workspace.id}`}>
                  {row.workspace.name}
                </Link>
              </td>
              {nodes.map((n) => {
                const cell = row.cells[n.id] ?? [];
                return (
                  <td key={n.id}>
                    {cell.length === 0 ? (
                      <span className="faint">—</span>
                    ) : (
                      cell.map((c) => (
                        <button
                          key={c.id}
                          className={`m-chip${isMissing(c) ? " ghost" : c.dirty ? " warn" : ""}`}
                          title={c.path}
                          onClick={(e) => {
                            const r = e.currentTarget.getBoundingClientRect();
                            menu.openAt(
                              r.left,
                              r.bottom + 2,
                              checkoutMenuItems(
                                c,
                                row.workspace,
                                actionsFor(row.workspace),
                              ),
                            );
                          }}
                        >
                          {c.kind === "worktree" ? (
                            <GitBranch size={10} />
                          ) : (
                            <FolderGit2 size={10} />
                          )}
                          {c.branch ?? "—"}
                          {c.sessions.length > 0 && (
                            <span className="m-chip-live">
                              ●{c.sessions.length}
                            </span>
                          )}
                        </button>
                      ))
                    )}
                  </td>
                );
              })}
            </tr>
          ))}
        </tbody>
      </table>
      {looseSection}
    </div>
  );
}

/** One session line: the live agent mark first (the signal), then the name,
 *  runtime, owner — and a status pill only when the status is NOT the routine
 *  "running". */
function SessionRow({
  s,
  agentState,
  meId,
}: {
  s: Session;
  agentState: Record<string, AgentState>;
  meId?: string;
}) {
  const agent = liveAgentMark(s.status, agentState[s.id])?.state;
  return (
    <div className="m-sess" data-testid={`session-${s.id}`}>
      {agent === "running" ? (
        <Loader2
          size={12}
          className="m-sess-mark running spin"
          data-testid={`agent-${s.id}`}
          aria-label="agent working"
        />
      ) : agent === "waiting" ? (
        <CircleDot
          size={12}
          className="m-sess-mark waiting"
          data-testid={`agent-${s.id}`}
          aria-label="agent waiting on you"
        />
      ) : (
        <SquareTerminal size={12} className="m-sess-mark idle" />
      )}
      <Link className="bright" to={`/sessions/${s.id}`}>
        {s.name}
      </Link>
      <Pill tone="accent">{s.runtime}</Pill>
      {s.status !== "running" && (
        <Pill tone={statusTone(s.status)}>{s.status}</Pill>
      )}
      {s.created_by && meId && s.created_by !== meId && (
        <span className="m-sess-owner" title="started by a teammate">
          team
        </span>
      )}
    </div>
  );
}
