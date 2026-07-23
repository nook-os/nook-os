import React, { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useParams } from "react-router-dom";
import {
  GitBranch,
  Trash2,
  PanelRightClose,
  PanelRightOpen,
  RefreshCw,
  RotateCw,
} from "lucide-react";
import { api, attachSession, type Session } from "@nookos/api";
import { Empty, Panel, Pill, statusTone, TerminalView } from "@nookos/ui";
import { useLive } from "../live";
import { useWorkspaceContext } from "../context";
import { ScopeChip } from "../layout";
import { SessionTabs } from "../SessionTabs";
import { SessionWindows, SplitButtons } from "../SessionWindows";
import { useSessionTabs } from "../sessionTabsStore";
import { askConfirm, notify } from "../dialogs";

const DIFF_PANEL_KEY = "nookos-diff-panel-open";

/**
 * Live git status for a checkout, or `null` when there is nothing to ask about.
 *
 * A hook rather than a query inside the panel, because the decision "is there a
 * git panel at all" belongs to the page: it sizes the grid column, and a column
 * sized for a panel that then declines to render is the blank space this fixes.
 * React Query dedupes the two call sites by key, so asking twice costs nothing.
 */
function useGitStatus(workspaceId: string | null, nodeId: string | undefined) {
  return useQuery({
    queryKey: ["git", workspaceId, nodeId],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/workspaces/{id}/git", {
        params: {
          path: { id: workspaceId! },
          query: { node_id: nodeId! },
        },
      });
      if (error) throw new Error(JSON.stringify(error));
      return data ?? null;
    },
    enabled: !!workspaceId && !!nodeId,
    refetchInterval: 10000,
    retry: false,
  });
}

function DiffView({ diff }: { diff: string }) {
  if (!diff.trim()) {
    return <Empty>Working tree is clean — no diff.</Empty>;
  }
  return (
    <div className="diff-view">
      {diff.split("\n").map((line, i) => {
        const cls = line.startsWith("+++") || line.startsWith("---")
          ? "file"
          : line.startsWith("diff --git")
            ? "file"
            : line.startsWith("@@")
              ? "hunk"
              : line.startsWith("+")
                ? "add"
                : line.startsWith("-")
                  ? "del"
                  : "";
        return (
          <div key={i} className={`diff-line ${cls}`}>
            {line || " "}
          </div>
        );
      })}
    </div>
  );
}

// Only rendered when the session has a workspace AND that checkout is a git
// repository — see `hasGitPanel` — so `workspaceId` is a plain string, not the
// nullable one off `session`.
function GitPanel({
  session,
  workspaceId,
}: {
  session: Session;
  workspaceId: string;
}) {
  const [tab, setTab] = useState<"diff" | "files">("diff");
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState<null | "commit" | "push">(null);
  const [note, setNote] = useState<string | null>(null);
  const { data, refetch, isFetching, error } = useGitStatus(
    workspaceId,
    session.node_id,
  );

  // Commit and push run git on the machine that holds the checkout — the same
  // place the diff above came from. The point is not to reimplement git in a
  // browser; it's that finishing the work you just read shouldn't require
  // finding a terminal and retyping the two commands you already decided on.
  const run = async (what: "commit" | "push") => {
    setBusy(what);
    setNote(null);
    const { data: result, error: err } =
      what === "commit"
        ? await api.POST("/api/v1/workspaces/{id}/git/commit", {
            params: { path: { id: workspaceId } },
            body: { node_id: session.node_id, message },
          })
        : await api.POST("/api/v1/workspaces/{id}/git/push", {
            params: { path: { id: workspaceId } },
            body: { node_id: session.node_id, credential_id: null },
          });
    setBusy(null);
    if (err) {
      setNote(typeof err === "string" ? err : JSON.stringify(err));
      return;
    }
    // The node answers with its own words — "committed 4f2a1c9", or git's
    // explanation of why not. Either way it's the truth, so show it verbatim.
    setNote(result?.message ?? null);
    if (result?.ok && what === "commit") setMessage("");
    refetch();
  };

  return (
    <Panel
      title={
        <>
          <GitBranch size={12} style={{ verticalAlign: "-2px" }} /> git ·{" "}
          <span className="bright">{data?.branch ?? "…"}</span>
        </>
      }
      actions={
        <>
          {data && (
            <Pill tone={data.dirty ? "warn" : "ok"}>
              {data.dirty ? `${data.files.length} changed` : "clean"}
            </Pill>
          )}{" "}
          <button
            className={`btn small${tab === "diff" ? " primary" : ""}`}
            onClick={() => setTab("diff")}
          >
            diff
          </button>{" "}
          <button
            className={`btn small${tab === "files" ? " primary" : ""}`}
            onClick={() => setTab("files")}
          >
            files
          </button>{" "}
          <button
            className="btn small"
            onClick={() => refetch()}
            disabled={isFetching}
            title="refresh"
          >
            <RefreshCw size={12} className={isFetching ? "spin" : ""} />
          </button>
        </>
      }
    >
      {data && (
        <div className="git-commit-bar">
          <input
            className="input"
            placeholder={
              data.dirty
                ? `commit message for ${data.files.length} changed file${data.files.length === 1 ? "" : "s"}`
                : "nothing to commit — working tree is clean"
            }
            value={message}
            onChange={(e) => setMessage(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && message.trim() && data.dirty) run("commit");
            }}
            disabled={!data.dirty || busy !== null}
          />
          <button
            className="btn primary small"
            onClick={() => run("commit")}
            disabled={!data.dirty || !message.trim() || busy !== null}
            title="stage everything and commit on the node"
          >
            {busy === "commit" ? "committing…" : "commit"}
          </button>
          <button
            className="btn small"
            onClick={() => run("push")}
            disabled={busy !== null}
            title={`push ${data.branch ?? "this branch"} to origin`}
          >
            {busy === "push" ? "pushing…" : "push"}
          </button>
        </div>
      )}
      {note && (
        <div className="git-commit-note small mono" onClick={() => setNote(null)}>
          {note}
        </div>
      )}

      {error ? (
        <Empty>git status unavailable: node offline?</Empty>
      ) : !data ? (
        <Empty>Loading…</Empty>
      ) : tab === "diff" ? (
        <DiffView diff={data.diff} />
      ) : data.files.length === 0 ? (
        <Empty>No changed files.</Empty>
      ) : (
        <table className="nook-table">
          <thead>
            <tr>
              <th>St</th>
              <th>Path</th>
            </tr>
          </thead>
          <tbody>
            {data.files.map((f) => (
              <tr key={f.path}>
                <td className="mono">
                  <Pill tone={f.status.includes("?") ? "info" : "warn"}>
                    {f.status.trim() || "·"}
                  </Pill>
                </td>
                <td className="mono">{f.path}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Panel>
  );
}

/** Live means the node still holds a terminal for it. */
function isLive(status: string): boolean {
  return status === "starting" || status === "running" || status === "detached";
}

export function SessionPage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [liveStatus, setLiveStatus] = useState<string | null>(null);
  const [attachKey, setAttachKey] = useState(0);
  const [gitOpen, setGitOpen] = useState(
    () => localStorage.getItem(DIFF_PANEL_KEY) !== "closed",
  );
  const sessionStatus = useLive((s) => s.sessionStatus);
  const openTab = useSessionTabs((s) => s.open);
  const closeTab = useSessionTabs((s) => s.close);

  const { data: session } = useQuery({
    queryKey: ["sessions", "one", id],
    queryFn: async () =>
      (await api.GET("/api/v1/sessions/{id}", { params: { path: { id: id! } } }))
        .data,
    enabled: !!id,
  });
  const { data: ws } = useQuery({
    queryKey: ["workspaces", session?.workspace_id],
    queryFn: async () =>
      (
        await api.GET("/api/v1/workspaces/{id}", {
          params: { path: { id: session!.workspace_id! } },
        })
      ).data,
    enabled: !!session?.workspace_id,
  });
  // Ad-hoc terminals name their machine instead of a workspace.
  const { data: nodes } = useQuery({
    queryKey: ["nodes"],
    queryFn: async () => (await api.GET("/api/v1/nodes")).data ?? [],
    enabled: !!session && !session.workspace_id,
  });
  const nodeName = nodes?.find((n) => n.id === session?.node_id)?.name;
  const git = useGitStatus(session?.workspace_id ?? null, session?.node_id);

  // Visiting a session opens (or refreshes) its tab, tagged with its
  // workspace so the strip can scope tabs to the workspace context.
  useEffect(() => {
    if (session) {
      openTab({
        id: session.id,
        name: session.name,
        runtime: session.runtime,
        workspaceId: session.workspace_id ?? undefined,
        workspaceName: ws?.name,
      });
    }
  }, [session, ws?.name, openTab]);

  // Opening a session from another workspace follows it: the switcher, tab
  // strip, board, and activity all move to that workspace's context. (An
  // explicit "all workspaces" context is left alone.)
  const selectWorkspace = useWorkspaceContext((s) => s.select);
  const selectedWorkspaceId = useWorkspaceContext((s) => s.selectedWorkspaceId);
  useEffect(() => {
    // An ad-hoc terminal has no workspace, so there is no context to follow to.
    if (
      session?.workspace_id &&
      selectedWorkspaceId &&
      selectedWorkspaceId !== session.workspace_id
    ) {
      selectWorkspace(session.workspace_id);
    }
  }, [session, selectedWorkspaceId, selectWorkspace]);

  if (!session) return <Empty>Loading…</Empty>;
  const status = liveStatus ?? sessionStatus[session.id] ?? session.status;

  // Two ways there is no git to show: an ad-hoc terminal, which has no
  // workspace, and a checkout that is not a repository — "+ New empty project"
  // makes one. `is_repo === false` is the only value that hides the panel:
  // while the first request is in flight, and if the node cannot be reached,
  // the answer is unknown, and guessing "no repo" would make the panel vanish
  // and come back on every reconnect.
  const hasGitPanel = !!session.workspace_id && git.data?.is_repo !== false;

  const toggleGit = () => {
    setGitOpen((open) => {
      localStorage.setItem(DIFF_PANEL_KEY, open ? "closed" : "open");
      return !open;
    });
  };

  const dead = status === "exited" || status === "error";

  const restart = async () => {
    setLiveStatus("starting");
    const { error } = await api.POST("/api/v1/sessions/{id}/restart", {
      params: { path: { id: session.id } },
    });
    if (error) {
      setLiveStatus(null);
      await notify("Restart failed", JSON.stringify(error));
      return;
    }
    // Remount the terminal so it re-attaches to the fresh tmux session.
    setAttachKey((k) => k + 1);
    queryClient.invalidateQueries();
  };

  const kill = async () => {
    // Say the blast radius out loud. Kill ends the whole tmux session, so a
    // session holding four terminals loses four terminals — which is a
    // surprise if you were only trying to get rid of the one in front of you.
    // (Closing a single terminal is the × on its chip.)
    const terminals =
      queryClient.getQueryData<{ index: number }[]>([
        "session-windows",
        session.id,
      ])?.length ?? 1;
    const ok = await askConfirm({
      title: "Kill session",
      description:
        terminals > 1
          ? `This session has ${terminals} terminals and ALL of them end — ` +
            "running processes are terminated on the node.\n\n" +
            "To close just one, use the × on its terminal chip."
          : "The tmux session ends for real on the node — running processes are terminated.",
      confirmLabel: terminals > 1 ? `kill all ${terminals}` : "kill",
      danger: true,
    });
    if (ok) {
      await api.POST("/api/v1/sessions/{id}/kill", {
        params: { path: { id: session.id } },
      });
      closeTab(session.id);
      navigate("/sessions");
    }
  };

  return (
    <div className="session-view">
      <SessionTabs activeId={session.id} />
      <div
        className="nook-grid"
        style={{
          gridTemplateColumns: gitOpen && hasGitPanel ? "1fr 440px" : "1fr",
          flex: 1,
          minHeight: 0,
        }}
      >
        <Panel
        title={
          <>
            {session.workspace_id ? (
              <Link to={`/workspaces/${session.workspace_id}`} className="bright">
                {ws?.name ?? "workspace"}
              </Link>
            ) : (
              // Ad-hoc terminal: no workspace, so name the machine it's on.
              <span className="bright">{nodeName ?? "terminal"}</span>
            )}
            <span className="faint"> ▸ </span>
            {session.name}
          </>
        }
        actions={
          <span
            style={{ display: "inline-flex", alignItems: "center", gap: 6 }}
          >
            {!dead && <SessionWindows sessionId={session.id} />}
            <Pill tone="accent">{session.runtime}</Pill>
            <Pill tone={statusTone(status)}>{status}</Pill>
            {dead ? (
              <button className="btn small" onClick={restart} title="restart session">
                <RotateCw size={12} /> restart
              </button>
            ) : (
              <SplitButtons sessionId={session.id} />
            )}
            {/* Nothing to diff — no workspace, or a checkout that is not a
                repository. Hiding the toggle as well as the panel matters: a
                button that opens an empty column is worse than no button. */}
            {hasGitPanel && (
              <button
                className="btn small icon"
                onClick={toggleGit}
                title={gitOpen ? "hide git panel" : "show git panel"}
              >
                {gitOpen ? <PanelRightClose size={13} /> : <PanelRightOpen size={13} />}
              </button>
            )}
            <button className="btn danger small" onClick={kill}>
              kill
            </button>
          </span>
        }
      >
          {dead ? (
            <div className="session-dead">
              <div className="session-dead-title">
                {session.error ? "This session couldn't start" : "This session has ended"}
              </div>
              {/* A session that never opened has a reason, and the reason is
                  usually the fix: a checkout that isn't there, a runtime that
                  isn't installed on that node. */}
              {session.error ? (
                <p className="muted small mono">{session.error}</p>
              ) : (
                <p className="muted small">
                  Its terminals are gone, but the tab, name and workspace are
                  kept. Restarting opens a fresh {session.runtime} session in
                  the same checkout.
                </p>
              )}
              <button className="btn primary" onClick={restart}>
                <RotateCw size={13} /> restart session
              </button>
            </div>
          ) : (
            <TerminalView
              key={`${session.id}:${attachKey}`}
              attach={(handlers) => attachSession(session.id, handlers)}
              onStatus={setLiveStatus}
            />
          )}
        </Panel>
        {gitOpen && hasGitPanel && session.workspace_id && (
          <GitPanel session={session} workspaceId={session.workspace_id} />
        )}
      </div>
    </div>
  );
}

export function SessionsPage() {
  const { selectedWorkspaceId, select } = useWorkspaceContext();
  const queryClient = useQueryClient();
  const closeTab = useSessionTabs((s) => s.close);
  const [filter, setFilter] = useState("");
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);

  const { data: sessions } = useQuery({
    queryKey: ["sessions", "all", selectedWorkspaceId],
    queryFn: async () =>
      (
        await api.GET("/api/v1/sessions", {
          params: {
            query: { workspace_id: selectedWorkspaceId ?? undefined },
          },
        })
      ).data ?? [],
  });
  // How many sessions the workspace scope is hiding. Scoping is useful, but a
  // silent filter is how a session started by an agent on another workspace
  // becomes "it never appeared" — the work exists, the page just refuses to
  // mention it. Cheap query; it is the same list without the filter.
  const { data: everySession } = useQuery({
    queryKey: ["sessions", "unscoped"],
    queryFn: async () => (await api.GET("/api/v1/sessions", {})).data ?? [],
    enabled: !!selectedWorkspaceId,
  });
  const sessionStatus = useLive((s) => s.sessionStatus);

  const all = sessions ?? [];
  const hiddenByScope = selectedWorkspaceId
    ? (everySession ?? []).filter((s) => !all.some((v) => v.id === s.id))
    : [];
  const q = filter.trim().toLowerCase();
  const shown = q
    ? all.filter((s) =>
        [s.name, s.runtime, sessionStatus[s.id] ?? s.status].some((v) =>
          v.toLowerCase().includes(q),
        ),
      )
    : all;
  const dead = shown.filter(
    (s) => !isLive(sessionStatus[s.id] ?? s.status),
  );

  const toggle = (id: string) =>
    setPicked((p) => {
      const next = new Set(p);
      if (!next.delete(id)) next.add(id);
      return next;
    });

  const removeMany = async (ids: string[], what: string) => {
    if (ids.length === 0) return;
    const ok = await askConfirm({
      title: `Delete ${ids.length} ${what}`,
      description:
        "Records are removed and any still-running tmux sessions are killed on their node.",
      confirmLabel: "delete",
      danger: true,
    });
    if (!ok) return;
    setBusy(true);
    for (const id of ids) {
      await api.DELETE("/api/v1/sessions/{id}", { params: { path: { id } } });
      closeTab(id);
    }
    setBusy(false);
    setPicked(new Set());
    queryClient.invalidateQueries();
  };

  const allShownPicked = shown.length > 0 && shown.every((s) => picked.has(s.id));

  return (
    <div className="session-view">
      <SessionTabs />
      <div
        className="nook-grid"
        style={{ gridTemplateColumns: "1fr", flex: 1, minHeight: 0 }}
      >
      <Panel
        title={`Sessions (${shown.length}${shown.length !== all.length ? ` of ${all.length}` : ""})`}
        actions={
          <span style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
            <input
              className="input small"
              style={{ width: 190 }}
              placeholder="search sessions…"
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
            />
            {picked.size > 0 && (
              <button
                className="btn danger small"
                disabled={busy}
                onClick={() => removeMany([...picked], "session(s)")}
              >
                <Trash2 size={12} /> delete {picked.size}
              </button>
            )}
            {picked.size === 0 && dead.length > 0 && (
              <button
                className="btn small"
                disabled={busy}
                title="delete every session that has already ended"
                onClick={() => removeMany(dead.map((s) => s.id), "ended session(s)")}
              >
                <Trash2 size={12} /> clean up {dead.length} ended
              </button>
            )}
            <ScopeChip />
          </span>
        }
      >
        {hiddenByScope.length > 0 && (
          <div className="scope-hidden-note small">
            <span>
              {hiddenByScope.length} session
              {hiddenByScope.length === 1 ? "" : "s"} in other workspaces
              {hiddenByScope.some((s) => isLive(sessionStatus[s.id] ?? s.status))
                ? " (some still running)"
                : ""}
              , hidden by the workspace scope.
            </span>
            <button className="btn small" onClick={() => select(null)}>
              show all
            </button>
          </div>
        )}
        {all.length === 0 ? (
          <Empty>
            {hiddenByScope.length > 0
              ? "No sessions in this workspace — the ones you have are elsewhere."
              : "No sessions yet — start one from a workspace."}
          </Empty>
        ) : shown.length === 0 ? (
          <Empty>Nothing matches “{filter}”.</Empty>
        ) : (
          <table className="nook-table">
            <thead>
              <tr>
                <th style={{ width: 28 }}>
                  <input
                    type="checkbox"
                    title="select all"
                    checked={allShownPicked}
                    onChange={() =>
                      setPicked(
                        allShownPicked ? new Set() : new Set(shown.map((s) => s.id)),
                      )
                    }
                  />
                </th>
                <th>Session</th>
                <th>Runtime</th>
                <th>Status</th>
                <th>Created</th>
                <th style={{ width: 40 }} />
              </tr>
            </thead>
            <tbody>
              {shown.map((s) => {
                const status = sessionStatus[s.id] ?? s.status;
                return (
                  <tr key={s.id} className={picked.has(s.id) ? "picked" : undefined}>
                    <td>
                      <input
                        type="checkbox"
                        checked={picked.has(s.id)}
                        onChange={() => toggle(s.id)}
                      />
                    </td>
                    <td>
                      <Link className="bright" to={`/sessions/${s.id}`}>
                        {s.name}
                      </Link>
                    </td>
                    <td>
                      <Pill tone="accent">{s.runtime}</Pill>
                    </td>
                    <td>
                      <Pill tone={statusTone(status)}>{status}</Pill>
                    </td>
                    <td className="muted small">
                      {new Date(s.created_at).toLocaleString()}
                    </td>
                    <td>
                      <button
                        className="btn danger small icon"
                        title="delete session"
                        disabled={busy}
                        onClick={() => removeMany([s.id], "session")}
                      >
                        <Trash2 size={12} />
                      </button>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </Panel>
      </div>
    </div>
  );
}

