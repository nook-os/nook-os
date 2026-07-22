import React, { useEffect, useRef, useState } from "react";
import { NavLink, Outlet, useLocation, useNavigate } from "react-router-dom";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Activity,
  Boxes,
  ChevronDown,
  Eye,
  FileText,
  FolderGit2,
  KanbanSquare,
  LayoutDashboard,
  LogOut,
  Mic,
  Server,
  Settings,
  SquareTerminal,
  MessageSquare,
  SlidersHorizontal,
  Users,
} from "lucide-react";
import { Plus } from "lucide-react";
import { api, type MeResponse } from "@nookos/api";
import { useLive } from "./live";
import { useWorkspaceContext } from "./context";
import { NewWorkHost } from "./NewWorkModal";
import { askText, DialogHost, notify } from "./dialogs";
import { JobsHud } from "./JobsHud";
import { useNewWork } from "./newwork";
import { FeedbackModalHost, useFeedbackModal } from "./FeedbackModal";

// Left rail: the permanent global nav. The top bar never repeats it — top is
// for CONTEXT (the selected workspace's views).
const SECTIONS = [
  { to: "/", label: "Dashboard", icon: LayoutDashboard, end: true },
  { to: "/workspaces", label: "Workspaces", icon: FolderGit2 },
  { to: "/sessions", label: "Sessions", icon: SquareTerminal },
  { to: "/board", label: "Board", icon: KanbanSquare },
  { to: "/activity", label: "Activity", icon: Activity },
  { to: "/nodes", label: "Nodes", icon: Server },
];

const COMING_SOON = [
  { label: "Team", icon: Users },
  { label: "Standup", icon: Mic },
];

/** Workspace-context tabs shown in the top bar once a workspace is chosen. */
function ContextTabs() {
  const { selectedWorkspaceId } = useWorkspaceContext();
  const location = useLocation();
  if (!selectedWorkspaceId) {
    return (
      <span className="faint small" style={{ padding: "0 6px" }}>
        pick a workspace to focus ↑
      </span>
    );
  }
  const overviewPath = `/workspaces/${selectedWorkspaceId}`;
  const tabs = [
    { to: overviewPath, label: "Overview", icon: Eye, active: location.pathname === overviewPath },
    { to: "/sessions", label: "Sessions", icon: SquareTerminal, active: location.pathname === "/sessions" },
    { to: "/board", label: "Board", icon: KanbanSquare, active: location.pathname === "/board" },
    { to: "/activity", label: "Activity", icon: Activity, active: location.pathname === "/activity" },
  ];
  return (
    <>
      {tabs.map((t) => (
        <NavLink key={t.label} to={t.to} className={`nook-tab${t.active ? " active" : ""}`}>
          <t.icon size={14} />
          {t.label}
        </NavLink>
      ))}
    </>
  );
}

function WorkspaceSwitcher() {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const location = useLocation();
  const { selectedWorkspaceId, select } = useWorkspaceContext();

  // Switching context STAYS on the current screen — scoping just updates in
  // place. Only detail routes of another entity re-target: a specific
  // workspace overview follows to the newly selected workspace, and a specific
  // session (which belongs to the old scope) falls back to the sessions list.
  const switchTo = (id: string | null) => {
    select(id);
    setOpen(false);
    const path = location.pathname;
    if (/^\/sessions\/.+/.test(path)) navigate("/sessions");
    else if (/^\/workspaces\/.+/.test(path) && id) navigate(`/workspaces/${id}`);
  };
  const { data: workspaces } = useQuery({
    queryKey: ["workspaces"],
    queryFn: async () => (await api.GET("/api/v1/workspaces")).data ?? [],
  });

  useEffect(() => {
    const close = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", close);
    return () => document.removeEventListener("mousedown", close);
  }, []);

  const current = (workspaces ?? []).find((w) => w.id === selectedWorkspaceId);

  // Renaming changes the label and nothing else — not the slug, not the
  // checkout on disk, not the remote. "acme/services" is the repo's name;
  // what you call it while you're working in it is your business.
  const rename = async () => {
    if (!current) return;
    const name = await askText({
      title: `Rename ${current.name}`,
      description:
        "Display name only. The folders on disk, the git remote and every " +
        "running session stay exactly where they are.",
      label: "Shown as",
      value: current.name,
      confirmLabel: "rename",
    });
    if (!name || name === current.name) return;
    const { error, response } = await api.PATCH("/api/v1/workspaces/{id}", {
      params: { path: { id: current.id } },
      body: { name },
    });
    if (error || !response.ok) {
      await notify("Could not rename", JSON.stringify(error));
      return;
    }
    queryClient.invalidateQueries({ queryKey: ["workspaces"] });
  };

  return (
    <div className="ws-switcher" ref={ref}>
      <button className="ws-switcher-btn" onClick={() => setOpen((o) => !o)}>
        <Boxes size={14} />
        <span className="slash">~/</span>
        <span className="name">{current?.name ?? "all workspaces"}</span>
        <ChevronDown size={13} />
      </button>
      {current && (
        <button
          className="ws-switcher-settings"
          title={`workspace settings — ${current.name}`}
          onClick={rename}
        >
          <SlidersHorizontal size={13} />
        </button>
      )}
      {open && (
        <div className="ws-switcher-menu">
          <button
            className={`ws-switcher-item${selectedWorkspaceId ? "" : " current"}`}
            onClick={() => switchTo(null)}
          >
            <Boxes size={14} /> all workspaces
          </button>
          {(workspaces ?? []).map((w) => (
            <button
              key={w.id}
              className={`ws-switcher-item${w.id === selectedWorkspaceId ? " current" : ""}`}
              onClick={() => switchTo(w.id)}
            >
              <FolderGit2 size={14} /> {w.name}
              <span className="faint small" style={{ marginLeft: "auto" }}>
                {w.locations.length}⨯
              </span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

/** Chip shown on pages currently scoped to the selected workspace. */
export function ScopeChip() {
  const { selectedWorkspaceId, select } = useWorkspaceContext();
  const { data: workspaces } = useQuery({
    queryKey: ["workspaces"],
    queryFn: async () => (await api.GET("/api/v1/workspaces")).data ?? [],
  });
  if (!selectedWorkspaceId) return null;
  const ws = (workspaces ?? []).find((w) => w.id === selectedWorkspaceId);
  return (
    <span className="scope-chip">
      ~/{ws?.name ?? "workspace"}
      <button title="clear workspace scope" onClick={() => select(null)}>
        ✕
      </button>
    </span>
  );
}

export function Shell({ me }: { me: MeResponse }) {
  const live = useLive();
  const navigate = useNavigate();
  const { data: nodes } = useQuery({
    queryKey: ["nodes"],
    queryFn: async () => (await api.GET("/api/v1/nodes")).data ?? [],
    refetchInterval: 30000,
  });
  const { data: sessions } = useQuery({
    queryKey: ["sessions", "active"],
    queryFn: async () =>
      (await api.GET("/api/v1/sessions", { params: { query: { active: true } } }))
        .data ?? [],
    refetchInterval: 30000,
  });

  const online = (nodes ?? []).filter((n) => n.status === "online").length;
  const activeSessions = (sessions ?? []).filter((s) =>
    ["running", "starting", "detached"].includes(s.status),
  );

  const showNewWork = useNewWork((s) => s.show);
  const showFeedback = useFeedbackModal((s) => s.show);

  const openTerminal = () => {
    const latest = activeSessions[0];
    navigate(latest ? `/sessions/${latest.id}` : "/sessions");
  };

  const logout = async () => {
    await api.POST("/api/v1/auth/logout");
    window.location.href = "/";
  };

  return (
    <div className="nook-app">
      <NewWorkHost />
      <FeedbackModalHost />
      <DialogHost />
      <JobsHud />
      <header className="nook-topbar">
        <div className="nook-brand">
          <span>◆</span>
          <span className="prompt">nook@os:~$</span>
          <span className="cursor" />
        </div>
        <button className="btn primary" onClick={() => showNewWork()}>
          <Plus size={14} style={{ verticalAlign: "-2px" }} /> New Work
        </button>
        <WorkspaceSwitcher />
        <nav className="nook-tabs">
          <ContextTabs />
          <span style={{ flex: 1 }} />
          {COMING_SOON.map((s) => (
            <span key={s.label} className="nook-tab soon" title="coming soon">
              <s.icon size={14} />
              {s.label}
              <span className="soon-badge">soon</span>
            </span>
          ))}
        </nav>
        <div className="nook-topbar-right">
          {/* Feedback lives here, spelled out, not just as one more unlabelled
              icon in the rail — you can't tell us what's wrong with a thing
              you can't find. */}
          <NavLink
            to="/feedback"
            className={({ isActive }) => `nook-tab${isActive ? " active" : ""}`}
            title="tell us what should be better"
          >
            <MessageSquare size={14} /> Feedback
          </NavLink>
          <NavLink
            to="/help"
            className={({ isActive }) => `nook-tab${isActive ? " active" : ""}`}
            title="how NookOS works"
          >
            <FileText size={14} /> Docs
          </NavLink>
          <button className="terminal-pill" onClick={openTerminal}>
            <SquareTerminal size={14} />
            terminal
            {activeSessions.length > 0 && <span>· {activeSessions.length}</span>}
          </button>
          <span className="bright">{me.user.display_name}</span>
          <span className="faint">{me.tenant.slug}</span>
          <button className="btn" onClick={logout} title="sign out">
            <LogOut size={13} />
          </button>
        </div>
      </header>

      <aside className="nook-rail">
        {SECTIONS.map((s) => (
          <NavLink
            key={s.to}
            to={s.to}
            end={s.end}
            data-tip={s.label}
            className={({ isActive }) =>
              `nook-rail-btn${isActive ? " active" : ""}`
            }
          >
            <s.icon size={19} />
          </NavLink>
        ))}
        <div className="spacer" />
        {/* Feedback sits where you reach for it once something annoys you —
            next to Settings, not buried in the nav list. It opens a modal
            rather than navigating, because the thought is usually one
            sentence and losing your place to write it down is the friction
            that stops people bothering. */}
        <button
          type="button"
          data-tip="Feedback"
          className="nook-rail-btn"
          onClick={() => showFeedback()}
        >
          <MessageSquare size={19} />
        </button>
        <NavLink
          to="/settings"
          data-tip="Settings"
          className={({ isActive }) => `nook-rail-btn${isActive ? " active" : ""}`}
        >
          <Settings size={19} />
        </NavLink>
      </aside>

      <main className="nook-main">
        <Outlet />
      </main>

      <footer className="nook-statusbar">
        <span>
          <span className={`dot ${live.connected ? "ok" : "err"}`} /> live
        </span>
        <span className="sep">│</span>
        <span>
          {online}/{(nodes ?? []).length} nodes online
        </span>
        <span className="sep">│</span>
        <span>{activeSessions.length} active sessions</span>
        <span className="sep">│</span>
        <span className="faint">tenant: {me.tenant.name}</span>
        <span style={{ flex: 1 }} />
        <span className="faint">NookOS 0.1.0</span>
      </footer>
    </div>
  );
}
