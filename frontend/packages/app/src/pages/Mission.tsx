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
  SquareTerminal,
} from "lucide-react";
import { api } from "@nookos/api";
import type { OverviewCheckout, OverviewWorkspace, Session } from "@nookos/api";
import { Empty, Panel, Pill, StatusDot, statusTone } from "@nookos/ui";
import { useNewWork } from "../newwork";
import { SessionOwner } from "../sessionOwner";
import { notify } from "../dialogs";
import { liveAgentMark, useLive, type AgentState } from "../live";
import {
  canAddWorktree,
  canOpenTerminal,
  deckStats,
  exceptionCounts,
  groupCheckoutsByNode,
  isMissing,
  loadCollapsed,
  overlayLive,
  repoRollup,
  saveCollapsed,
  visibleRepos,
  type Lamp,
} from "../mission";

/// Mission Control (MAIN-226): every repo × machine × checkout × session on one
/// dense screen. An annunciator deck on top — fleet counters, live agent chips,
/// exception lamps that double as filters — and a repo-first collapsible tree
/// below it. Badges appear only for exceptions; the routine renders quiet.
export function MissionPage() {
  const showNewWork = useNewWork((s) => s.show);
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [q, setQ] = useState("");
  const [lamp, setLamp] = useState<Lamp | null>(null);
  const [ghosts, setGhosts] = useState(false);
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

  return (
    <div
      className="nook-grid"
      style={{ gridTemplateColumns: "1fr", gridTemplateRows: "1fr" }}
    >
      <Panel
        title="Mission Control"
        actions={
          <span
            style={{ display: "inline-flex", alignItems: "center", gap: 6 }}
          >
            <input
              className="input small mono"
              placeholder="filter repo / node / branch / session…"
              value={q}
              onChange={(e) => setQ(e.target.value)}
              style={{ width: 260 }}
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
        ) : (
          <div className="m-tree">
            {repos.map(({ workspace: w, checkouts, hiddenGhosts }) => (
              <RepoSection
                key={w.id}
                w={w}
                checkouts={checkouts}
                hiddenGhosts={hiddenGhosts}
                collapsed={!filtering && collapsed.has(w.id)}
                onToggle={() => toggleRepo(w.id)}
                agentState={agentState}
                meId={meId}
                onTerminal={(c) => terminalHere(w, c)}
                onWorktree={(c) =>
                  showNewWork({
                    workspaceId: w.id,
                    nodeId: c.node_id,
                    worktree: true,
                  })
                }
                onShowGhosts={() => setGhosts(true)}
              />
            ))}
            {(live?.loose_sessions.length ?? 0) > 0 && !lamp && (
              <section className="m-repo" data-testid="loose-sessions">
                <div className="m-repo-head static">
                  <SquareTerminal size={13} className="m-repo-icon" />
                  <span className="m-repo-name muted">ad-hoc terminals</span>
                  <span className="m-repo-remote">no workspace</span>
                </div>
                <div className="m-node">
                  {live!.loose_sessions.map((s) => (
                    <SessionRow
                      key={s.id}
                      s={s}
                      agentState={agentState}
                      meId={meId}
                    />
                  ))}
                </div>
              </section>
            )}
          </div>
        )}
      </Panel>
    </div>
  );
}

function RepoSection({
  w,
  checkouts,
  hiddenGhosts,
  collapsed,
  onToggle,
  agentState,
  meId,
  onTerminal,
  onWorktree,
  onShowGhosts,
}: {
  w: OverviewWorkspace;
  checkouts: OverviewCheckout[];
  hiddenGhosts: number;
  collapsed: boolean;
  onToggle: () => void;
  agentState: Record<string, AgentState>;
  meId?: string;
  onTerminal: (c: OverviewCheckout) => void;
  onWorktree: (c: OverviewCheckout) => void;
  onShowGhosts: () => void;
}) {
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
                  <span className="m-co-branch">{c.branch ?? "—"}</span>
                  <span className="m-co-path" title={c.path}>
                    {c.path}
                  </span>
                  {isMissing(c) ? (
                    <Pill
                      tone="warn"
                      title="this checkout has vanished from disk"
                    >
                      missing
                    </Pill>
                  ) : c.dirty ? (
                    <Pill tone="warn" title="uncommitted changes">
                      dirty
                    </Pill>
                  ) : null}
                  <span className="m-co-actions">
                    {canOpenTerminal(c) && (
                      <button
                        className="btn small"
                        data-testid={`terminal-${c.id}`}
                        title="open a terminal in this checkout"
                        onClick={() => onTerminal(c)}
                      >
                        <SquareTerminal size={12} /> terminal here
                      </button>
                    )}
                    {canAddWorktree(c) && (
                      <button
                        className="btn small"
                        data-testid={`worktree-${c.id}`}
                        title="cut a new worktree from this clone"
                        onClick={() => onWorktree(c)}
                      >
                        + worktree
                      </button>
                    )}
                  </span>
                </div>
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
      <span className="m-sess-owner">
        <SessionOwner createdBy={s.created_by} meId={meId} />
      </span>
    </div>
  );
}
