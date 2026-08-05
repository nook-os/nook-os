import React, { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, Navigate, useNavigate, useParams } from "react-router-dom";
import {
  GitBranch,
  Trash2,
  PanelRightClose,
  PanelRightOpen,
  RefreshCw,
  RotateCw,
  List,
  SquareTerminal,
  X,
} from "lucide-react";
import { api, attachSession, type Session } from "@nookos/api";
import {
  Empty,
  Panel,
  Pill,
  RowAction,
  RowActions,
  statusTone,
  TerminalView,
  type TerminalControls,
} from "@nookos/ui";
import {
  canReadClipboardNow,
  ContextMenuRegion,
  type ContextMenuItem,
} from "../contextMenu";
import {
  commitPaths,
  committable,
  diffFor,
  isConflict,
  splitDiffByFile,
  treeState,
} from "../gitPanelModel";
import { useLive } from "../live";
import { useWorkspaceContext } from "../context";
import { ScopeChip } from "../layout";
import { useWorkingSet } from "../workingSetTabs";
import { SessionShell } from "../SessionNavigator";
import { SessionTabs } from "../SessionTabs";
import { SessionWindows, SplitButtons } from "../SessionWindows";
import { SessionOwner } from "../sessionOwner";
import { askConfirm, notify } from "../dialogs";
import { recall, remember } from "../lastPlace";

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

/// One file's slice of the working-tree diff (MAIN-325 AC-2).
///
/// A file with no slice is the interesting case, not an edge case: an untracked
/// file has no diff at all, because git has nothing to compare it against. An
/// empty box there reads as "no changes", which is the opposite of the truth.
function FileDiff({
  file,
  sections,
}: {
  file: { status: string; path: string };
  sections: Record<string, string>;
}) {
  const result = diffFor(file, sections);
  return (
    <div className="git-file-diff">
      <div className="git-file-diff-head mono small">{file.path}</div>
      {"diff" in result ? (
        <DiffView diff={result.diff} />
      ) : (
        <Empty>{result.reason}</Empty>
      )}
    </div>
  );
}

// Only rendered when the session has a workspace AND that checkout is a git
// repository — see `hasGitPanel` — so `workspaceId` is a plain string, not the
// nullable one off `session`.
function GitPanel({
  session,
  workspaceId,
  onClose,
}: {
  session: Session;
  workspaceId: string;
  /** Mobile only: the panel is a drawer there, and a drawer needs a close of
   *  its own — the header toggle that opened it is underneath it. */
  onClose: () => void;
}) {
  const [tab, setTab] = useState<"diff" | "files">("files");
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState<null | "commit" | "push">(null);
  const [note, setNote] = useState<string | null>(null);
  // Which file's diff is showing (AC-2), and which files the next commit takes
  // (AC-1). `staged === null` means "everything", which is what commit did
  // before it could be told otherwise — an empty Set means the user has
  // deselected every file, which is a different thing and must not silently
  // commit the tree.
  const [openFile, setOpenFile] = useState<string | null>(null);
  const [staged, setStaged] = useState<Set<string> | null>(null);
  const { data, refetch, isFetching, error } = useGitStatus(
    workspaceId,
    session.node_id,
  );

  const files = data?.files ?? [];
  const state = treeState(files);
  const canCommit = committable(files);
  const sections = React.useMemo(() => splitDiffByFile(data?.diff ?? ""), [data?.diff]);
  // The selection is over paths that still exist; a file committed or reverted
  // elsewhere must not keep a vote in the next commit.
  const selected = staged
    ? canCommit.filter((f) => staged.has(f.path)).map((f) => f.path)
    : canCommit.map((f) => f.path);
  const shown = openFile ? files.find((f) => f.path === openFile) : undefined;

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
            body: {
              node_id: session.node_id,
              message,
              // Whole tree → `null` (what every caller sent before selective
              // staging); anything less → the explicit list. Never
              // `canCommit.length` here: see `commitPaths`.
              paths: commitPaths(files, selected),
            },
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
    if (result?.ok && what === "commit") {
      setMessage("");
      // The committed files are gone from the tree; a stale selection would
      // otherwise carry their paths into the next commit.
      setStaged(null);
    }
    refetch();
  };

  return (
    <Panel
      className="git-panel"
      title={
        <>
          <GitBranch size={12} style={{ verticalAlign: "-2px" }} /> git ·{" "}
          <span className="bright" title={data?.branch ?? undefined}>
            {data?.branch ?? "…"}
          </span>
        </>
      }
      actions={
        <>
          {data && (
            <Pill
              tone={
                state === "conflict" ? "err" : state === "uncommitted" ? "warn" : "ok"
              }
              title={
                state === "conflict"
                  ? "merge conflict — resolve the marked files before committing"
                  : undefined
              }
            >
              {state === "conflict"
                ? `${files.filter(isConflict).length} conflicted`
                : state === "uncommitted"
                  ? `${files.length} uncommitted`
                  : "clean"}
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
          {/* Hidden on desktop by CSS — there the header toggle stays visible
              beside the panel and a second close would be clutter. */}
          <button
            className="btn small icon git-drawer-close"
            onClick={onClose}
            title="close git panel"
          >
            <X size={13} />
          </button>
        </>
      }
    >
      {data && state === "conflict" && (
        <div className="git-conflict-note small">
          <b>Merge conflict.</b> {files.filter(isConflict).length} file
          {files.filter(isConflict).length === 1 ? " has" : "s have"} unresolved
          markers. Resolve them in the editor — staging a conflicted file from
          here would mark it resolved with the markers still in it.
        </div>
      )}
      {data && (
        <div className="git-commit-bar">
          <input
            className="input"
            placeholder={
              canCommit.length === 0
                ? state === "conflict"
                  ? "resolve the conflicts first"
                  : "nothing to commit — working tree is clean"
                : selected.length === canCommit.length
                  ? `commit message for ${canCommit.length} file${canCommit.length === 1 ? "" : "s"}`
                  : `commit message for ${selected.length} of ${canCommit.length} files`
            }
            value={message}
            onChange={(e) => setMessage(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && message.trim() && selected.length > 0)
                run("commit");
            }}
            disabled={selected.length === 0 || busy !== null}
          />
          <button
            className="btn primary small"
            onClick={() => run("commit")}
            disabled={selected.length === 0 || !message.trim() || busy !== null}
            title={
              selected.length === canCommit.length
                ? "stage everything and commit on the node"
                : `stage and commit ${selected.length} selected file${selected.length === 1 ? "" : "s"}`
            }
          >
            {busy === "commit"
              ? "committing…"
              : selected.length === canCommit.length
                ? "commit"
                : `commit ${selected.length}`}
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

      {/* The one scroller in this panel. Both tabs live inside it, so the
          commit bar and any note stay put while only the content moves —
          and there is exactly one scrollbar rather than the panel body and
          the diff each growing their own. */}
      <div className="git-panel-content">
      {error ? (
        <Empty>git status unavailable: node offline?</Empty>
      ) : !data ? (
        <Empty>Loading…</Empty>
      ) : tab === "diff" ? (
        /* The whole-tree diff, as before. Per-file lives on the files tab,
           where the file you clicked is right above it. */
        <DiffView diff={data.diff} />
      ) : files.length === 0 ? (
        <Empty>No changed files.</Empty>
      ) : (
        <>
          <table className="nook-table git-files">
            <thead>
              <tr>
                <th style={{ width: 24 }}>
                  <input
                    type="checkbox"
                    title="select every file"
                    checked={
                      canCommit.length > 0 && selected.length === canCommit.length
                    }
                    ref={(el) => {
                      // Partially selected reads as neither on nor off, which
                      // is exactly what it is.
                      if (el)
                        el.indeterminate =
                          selected.length > 0 && selected.length < canCommit.length;
                    }}
                    disabled={canCommit.length === 0}
                    onChange={() =>
                      setStaged(
                        selected.length === canCommit.length ? new Set() : null,
                      )
                    }
                  />
                </th>
                <th>St</th>
                <th>Path</th>
              </tr>
            </thead>
            <tbody>
              {files.map((f) => {
                const conflicted = isConflict(f);
                return (
                  <tr
                    key={f.path}
                    className={`git-file-row${openFile === f.path ? " active" : ""}`}
                  >
                    <td>
                      <input
                        type="checkbox"
                        checked={!conflicted && selected.includes(f.path)}
                        disabled={conflicted}
                        title={
                          conflicted
                            ? "resolve this conflict before committing it"
                            : "include in the next commit"
                        }
                        onChange={() =>
                          setStaged((prev) => {
                            const next = new Set(prev ?? canCommit.map((x) => x.path));
                            if (!next.delete(f.path)) next.add(f.path);
                            return next;
                          })
                        }
                      />
                    </td>
                    <td className="mono">
                      <Pill
                        tone={
                          conflicted ? "err" : f.status.includes("?") ? "info" : "warn"
                        }
                      >
                        {f.status.trim() || "·"}
                      </Pill>
                    </td>
                    {/* AC-2: the path is the control — clicking it opens this
                        file's diff below, clicking again closes it. */}
                    <td className="mono">
                      <button
                        className="git-file-path"
                        onClick={() =>
                          setOpenFile((cur) => (cur === f.path ? null : f.path))
                        }
                        title="show this file's diff"
                      >
                        {f.path}
                      </button>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
          {shown && <FileDiff file={shown} sections={sections} />}
        </>
      )}
      </div>
    </Panel>
  );
}

/** Live means the node still holds a terminal for it. */
function isLive(status: string): boolean {
  return status === "starting" || status === "running" || status === "detached";
}

/**
 * Where a session actually runs: the node holding the checkout and the branch
 * it sits on. Ad-hoc `$HOME` terminals have no checkout, so callers only render
 * this when `session.checkout` is present. A linked git worktree is flagged so
 * a session on a throwaway branch reads differently from one on the primary
 * clone. Reuses the same amber pills as the rest of the header — no new widget.
 */
function CheckoutChip({
  checkout,
}: {
  checkout: NonNullable<Session["checkout"]>;
}) {
  const worktree = checkout.kind === "worktree";
  return (
    <span className="checkout-chip">
      <Pill title={`node: ${checkout.node_name}`}>{checkout.node_name}</Pill>
      {checkout.branch && (
        <Pill
          title={`branch: ${checkout.branch}${worktree ? " (worktree)" : ""}`}
        >
          <GitBranch size={11} style={{ verticalAlign: "-1px" }} />
          {checkout.branch}
          {worktree && <span className="faint">·wt</span>}
        </Pill>
      )}
    </span>
  );
}

// The session terminal's right-click menu (AC-4): copy the terminal's own
// selection, or paste clipboard text down the session's existing send path
// (term.paste → onData → transport.sendInput). Registered as a context-menu
// region so it beats the generic fallback on the app's highest-traffic surface.
function terminalMenuItems(controls: TerminalControls | null): ContextMenuItem[] {
  const hasSelection = !!controls && controls.hasSelection();
  const paste: ContextMenuItem = canReadClipboardNow()
    ? {
        label: "Paste to session",
        onSelect: () => {
          navigator.clipboard
            .readText()
            .then((text) => controls?.paste(text))
            .catch(() => {});
        },
      }
    : {
        label: "Paste to session",
        disabled: true,
        hint: "Clipboard access blocked",
      };
  return [
    {
      label: "Copy selection",
      disabled: !hasSelection,
      onSelect: () => controls?.copySelection(),
    },
    paste,
  ];
}

export function SessionPage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [liveStatus, setLiveStatus] = useState<string | null>(null);
  const [attachKey, setAttachKey] = useState(0);
  const [termControls, setTermControls] = useState<TerminalControls | null>(null);
  const [gitOpen, setGitOpen] = useState(
    () => localStorage.getItem(DIFF_PANEL_KEY) !== "closed",
  );
  const sessionStatus = useLive((s) => s.sessionStatus);

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

  // A stopped session is NOT dead — it is parked, and the same "restart"
  // control brings it back — but it does share the "nothing to attach to"
  // shape, so it gets the same affordance under a different name (AC-2/AC-6).
  const stopped = status === "stopped";
  const dead = status === "exited" || status === "error";
  // The node holds no live WebSocket — a seeded/synthetic node that reads
  // `online` in the database but never connected, or a real node that dropped.
  // The row can still say `running`, so this is the ONLY honest signal that the
  // terminal cannot stream. Rendering it as a distinct state is what stops the
  // blank terminal + the `/windows` retry-storm behind it: there is nothing on
  // the other end to attach to or poll.
  const offline = !dead && session.node_online === false;

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
      // No tab to close: the strip is the live session list, so refreshing it
      // is what removes the tab (MAIN-322).
      queryClient.invalidateQueries({ queryKey: ["sessions"] });
      navigate("/sessions");
    }
  };

  return (
    <SessionShell activeId={session.id}>
      <RememberSession sessionId={session.id} />
      <OpenInWorkingSet sessionId={session.id} />
      <SessionTabs activeId={session.id} />
      <div
        className="nook-grid session-grid"
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
            {!dead && !offline && <SessionWindows sessionId={session.id} />}
            <Pill tone="accent">{session.runtime}</Pill>
            <Pill tone={offline ? "err" : statusTone(status)}>
              {offline ? "node offline" : status}
            </Pill>
            {session.checkout && <CheckoutChip checkout={session.checkout} />}
            {stopped ? (
              <button className="btn small" onClick={restart} title="start this session again">
                <RotateCw size={12} /> start
              </button>
            ) : dead ? (
              <button className="btn small" onClick={restart} title="restart session">
                <RotateCw size={12} /> restart
              </button>
            ) : offline ? null : (
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
            {/* AC-3: `/sessions` opens a session now, so the inventory needs a
                door of its own. Here, because this is the screen the nav
                actually lands on. */}
            <Link className="btn small" to="/sessions/list" title="all sessions">
              <List size={12} /> all
            </Link>
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
          ) : offline ? (
            // The honest node-offline state. Not a terminal that will never
            // paint and not a `/windows` poll that only 400s — the node is not
            // connected, so we say that and stop hammering it. tmux keeps the
            // session on the node (if there is a real one), so it resumes on
            // reconnect with no restart.
            <div className="session-dead">
              <div className="session-dead-title">This session&apos;s node is offline</div>
              <p className="muted small">
                {nodeName ? <><span className="mono">{nodeName}</span> isn&apos;t </> : "Its node isn't "}
                connected right now, so the terminal can&apos;t stream. Nothing is
                lost — it resumes on its own when the node comes back.
              </p>
              <button
                className="btn"
                onClick={() =>
                  queryClient.invalidateQueries({ queryKey: ["sessions", "one", id] })
                }
              >
                <RotateCw size={13} /> check again
              </button>
            </div>
          ) : (
            <ContextMenuRegion
              // `display: contents` keeps this wrapper out of the layout so the
              // terminal still fills the panel; it stays in the DOM tree so the
              // context-menu resolver finds the region on right-click.
              style={{ display: "contents" }}
              items={() => terminalMenuItems(termControls)}
            >
              <TerminalView
                key={`${session.id}:${attachKey}`}
                attach={(handlers) => attachSession(session.id, handlers)}
                onStatus={setLiveStatus}
                onControls={setTermControls}
              />
            </ContextMenuRegion>
          )}
        </Panel>
        {gitOpen && hasGitPanel && session.workspace_id && (
          <>
            {/* Exists only for the phone layout, where the git panel is a
                drawer over the terminal: tapping the dimmed terminal closes
                it. Display: none on desktop, so it can never eat a click
                there. */}
            <div
              className="git-drawer-scrim"
              aria-hidden="true"
              onClick={toggleGit}
            />
            <GitPanel
              session={session}
              workspaceId={session.workspace_id}
              onClose={toggleGit}
            />
          </>
        )}
      </div>
    </SessionShell>
  );
}

/// What `/sessions` renders now (MAIN-321): the first session, not a list.
///
/// Clicking Sessions used to land on an inventory, and the thing you wanted was
/// always one more click away. "First" is deliberately the FIRST TAB — the same
/// order the strip renders, from the same hook — so the nav never opens a
/// session that is visibly not the leftmost tab.
///
/// The redirect REPLACES the history entry. Pushing it would put `/sessions`
/// behind every session you open, so Back would bounce you straight forward
/// again and the button would look broken.
/** Records the session you are looking at, so returning to `/sessions` comes
 *  back here instead of to whichever tab happens to be first.
 *
 *  Its own component, and self-contained: it needs the tenant, `SessionPage`
 *  does not otherwise, and the `["me"]` query is deduped across the app so
 *  asking here costs nothing. Renders nothing.
 *
 *  Written on VIEW rather than on navigation, so it also captures a tab you
 *  landed on by any other route — a deep link, a restart, the strip. */
/** Opening a session ADDS it to the working set (MAIN-417 AC-2).
 *
 *  On VIEW rather than on click, for the same reason `RememberSession` is: the
 *  pane, a deep link, a restart and the keyboard all land here, and a set that
 *  only the pane could add to would be empty for everyone who arrived any other
 *  way. Idempotent — the hook writes nothing when the session is already open,
 *  so this costs one request the first time you look at a session and none
 *  afterwards. */
function OpenInWorkingSet({ sessionId }: { sessionId: string }) {
  const { open, loaded } = useWorkingSet();
  React.useEffect(() => {
    // Not before the stored set has arrived: opening against an assumed-empty
    // set would write a strip containing only this session and lose the rest.
    if (loaded) open(sessionId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId, loaded]);
  return null;
}

function RememberSession({ sessionId }: { sessionId: string }) {
  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
  const tenantId = me?.tenant?.id;
  React.useEffect(() => {
    remember(tenantId, "session.id", sessionId);
  }, [tenantId, sessionId]);
  return null;
}

export function SessionsIndex() {
  // The WORKING SET, not the live list (MAIN-417): "/sessions opens the first
  // tab" has to mean the first tab you actually have, and after the upgrade
  // that set starts empty (AC-5) — landing on a session you never opened would
  // be the old derived-strip behaviour wearing a new name.
  const { tabs, loaded } = useWorkingSet();
  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
  const tenantId = me?.tenant?.id;

  // Deciding before the list arrives is how you flash "no sessions yet" at
  // somebody who has ten, so wait for the answer rather than guess at it.
  if (!loaded) return <div className="session-view" />;

  if (tabs.length > 0) {
    // Back to where you were, not to whatever happens to be first. Checked
    // against the LIVE tabs: a remembered session that has since exited is
    // gone, and navigating to it would land on a dead page — so a stale memory
    // falls back to the first tab rather than failing.
    const wanted = recall(tenantId, "session.id");
    const target = tabs.find((t) => t.id === wanted) ?? tabs[0];
    return <Navigate to={`/sessions/${target.id}`} replace />;
  }

  return (
    // No <SessionTabs/> here: with no tabs it renders nothing, and mounting it
    // would only run the same hook a second time. The navigator DOES render —
    // "no sessions in this workspace" and "no sessions anywhere" are different
    // answers, and the pane is where the second one is visible.
    <SessionShell>
      <div className="nook-grid" style={{ gridTemplateColumns: "1fr", flex: 1, minHeight: 0 }}>
        <Panel
          title="Sessions"
          actions={
            <span style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
              <Link className="btn small" to="/sessions/list">
                <List size={12} /> all sessions
              </Link>
              <ScopeChip />
            </span>
          }
        >
          <Empty>
            <span
              style={{ display: "inline-flex", alignItems: "center", gap: 8 }}
            >
              <SquareTerminal size={14} />
              No tabs open — pick a session from the pane, or start one with{" "}
              <b>+ New Workspace</b>.
            </span>
          </Empty>
        </Panel>
      </div>
    </SessionShell>
  );
}

export function SessionsPage() {
  const { selectedWorkspaceId, select } = useWorkspaceContext();
  const queryClient = useQueryClient();
  const [filter, setFilter] = useState("");
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);

  // Who the caller is, so an owner/admin (who sees the whole tenant's sessions)
  // can tell theirs apart from the team's. Deduped on the ["me"] key.
  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
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
    }
    setBusy(false);
    setPicked(new Set());
    queryClient.invalidateQueries();
  };

  const allShownPicked = shown.length > 0 && shown.every((s) => picked.has(s.id));

  return (
    <SessionShell>
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
                <th>Owner</th>
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
                      {s.checkout && <CheckoutChip checkout={s.checkout} />}
                    </td>
                    <td>
                      <Pill tone="accent">{s.runtime}</Pill>
                    </td>
                    <td>
                      <Pill tone={statusTone(status)}>{status}</Pill>
                    </td>
                    <td>
                      <SessionOwner createdBy={s.created_by} meId={me?.user?.id} />
                    </td>
                    <td className="muted small">
                      {new Date(s.created_at).toLocaleString()}
                    </td>
                    <td>
                      <RowActions>
                        <RowAction
                          icon={Trash2}
                          danger
                          title="delete this session"
                          disabled={busy}
                          onClick={() => removeMany([s.id], "session")}
                        />
                      </RowActions>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </Panel>
      </div>
    </SessionShell>
  );
}

