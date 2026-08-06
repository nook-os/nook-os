import React, { useEffect, useRef, useState } from "react";
import { NotificationBell } from "./Notifications";
import { PendingInteractions } from "./Interactions";
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
  Radar,
  Mic,
  NotebookText,
  Server,
  ShieldCheck,
  Settings,
  SquareTerminal,
  MessageSquare,
  SlidersHorizontal,
  Users,
} from "lucide-react";
import { Plus } from "lucide-react";
import { api, listChannels, listDms, type MeResponse } from "@nookos/api";
import { useLive } from "./live";
import { TenantSwitcher } from "./TenantSwitcher";
import { useControlPlaneVersion } from "./NodeFacts";
import { useWorkspaceContext } from "./context";
import { NewWorkHost } from "./NewWorkModal";
import { ControlPlanePill } from "./ControlPlanePill";
import { ControlPlaneTabs } from "./ControlPlaneTabs";
import { askText, DialogHost, notify } from "./dialogs";
import { JobsHud } from "./JobsHud";
import { useNewWork } from "./newwork";
import { FeedbackModalHost, useFeedbackModal } from "./FeedbackModal";

// Left rail: the permanent global nav. The top bar never repeats it — top is
// for CONTEXT (the selected workspace's views).
const SECTIONS = [
  { to: "/", label: "Dashboard", icon: LayoutDashboard, end: true },
  { to: "/mission", label: "Mission Control", icon: Radar },
  // Sessions above Workspaces: this is where the work actually happens, so it
  // is the rail's most-clicked destination by a distance.
  { to: "/sessions", label: "Sessions", icon: SquareTerminal },
  { to: "/workspaces", label: "Workspaces", icon: FolderGit2 },
  { to: "/board", label: "Board", icon: KanbanSquare },
  { to: "/chat", label: "Chat", icon: MessageSquare },
  { to: "/nodes", label: "Nodes", icon: Server },
  // Person-global, not workspace-scoped: your private notebook follows you
  // across every org you belong to (MAIN-101).
  { to: "/notebook", label: "Notes", icon: NotebookText },
];

/// The management surface — the Activity table lives here as a section now,
/// alongside the operator's fleet views. Shown to a tenant admin/owner or an
/// operator. (Workspaces went back to the rail after a day here: they are what
/// the app is ABOUT, and hiding the product inside Admin was the wrong read.)
///
/// Absent rather than disabled: a greyed-out door still tells you there is a
/// room, and for the operator half the room is other people's fleets.
const ADMIN_SECTION = {
  to: "/admin",
  label: "Admin",
  icon: ShieldCheck,
  end: false,
};

const COMING_SOON = [{ label: "Standup", icon: Mic }];

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
    // Prefix, not equality: `/sessions` redirects to a session now (MAIN-321),
    // so an exact match would mean this tab is never the active one.
    {
      to: "/sessions",
      label: "Sessions",
      icon: SquareTerminal,
      active: location.pathname.startsWith("/sessions"),
    },
    { to: "/board", label: "Board", icon: KanbanSquare, active: location.pathname === "/board" },
    {
      to: "/admin?section=activity",
      label: "Activity",
      icon: Activity,
      active: location.pathname === "/admin" && location.search.includes("section=activity"),
    },
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
  // session (which belongs to the old scope) goes back to `/sessions` — which
  // since MAIN-321 lands on the first session of the NEW scope, or its empty
  // state, rather than on a list.
  // Takes an id, never null. "All workspaces" is gone (it stopped meaning
  // anything once the strip became a working set), so the only way to change
  // context is to name a workspace.
  const switchTo = (id: string) => {
    select(id);
    setOpen(false);
    const path = location.pathname;
    // `/sessions/list` is excluded on purpose: it is a LIST, which rescopes in
    // place like every other list, not a detail route belonging to the old
    // scope. Sending it to `/sessions` would open a session instead.
    if (/^\/sessions\/.+/.test(path) && path !== "/sessions/list") navigate("/sessions");
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
      <button className="ws-switcher-btn" title="switch workspace" onClick={() => setOpen((o) => !o)}>
        <Boxes size={14} />
        <span className="slash">~/</span>
        <span className="name">{current?.name ?? "pick a workspace"}</span>
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

  // Total chat unread for the rail badge (MAIN-117 AC-5). Shares the Chat page's
  // query keys, so when chat is open the live stream's invalidations bump this
  // badge too; off the chat page, refetch-on-focus keeps it fresh (no polling).
  const { data: chatChannels } = useQuery({
    queryKey: ["chat", "channels"],
    queryFn: () => listChannels(),
    refetchOnWindowFocus: true,
  });
  const { data: chatDms } = useQuery({
    queryKey: ["chat", "dms"],
    queryFn: () => listDms(),
    refetchOnWindowFocus: true,
  });
  const chatUnread =
    (chatChannels ?? []).reduce((n, c) => n + (c.unread_count ?? 0), 0) +
    (chatDms ?? []).reduce((n, d) => n + (d.unread_count ?? 0), 0);

  const online = (nodes ?? []).filter((n) => n.status === "online").length;
  const cpVersion = useControlPlaneVersion();
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

  // Absent unless held — see ADMIN_SECTION.
  const isAdmin =
    !!me.capability?.operator || me.user.role === "owner" || me.user.role === "admin";
  const sections = isAdmin ? [...SECTIONS, ADMIN_SECTION] : SECTIONS;

  return (
    <div className="nook-app">
      <NewWorkHost />
      <FeedbackModalHost />
      <DialogHost />
      <JobsHud />
      {/* Desktop-only control-plane tab strip, above the top bar. Renders null
          on the web build, so the layout there is unchanged (AC-1/NG-5). */}
      <ControlPlaneTabs />
      <header className="nook-topbar">
        <div className="nook-brand">
          <span>◆</span>
          <span className="prompt">nook@os:~$</span>
          <span className="cursor" />
        </div>
        <button className="btn primary" onClick={() => showNewWork()}>
          <Plus size={14} style={{ verticalAlign: "-2px" }} /> New Workspace
        </button>
        <ControlPlanePill />
        {/* Team first, then workspace: a workspace belongs to a team, so the
            wider scope reads to the left of the narrower one — and the control
            people reach for most often is nearest the brand. */}
        <TenantSwitcher me={me} />
        <WorkspaceSwitcher />
        <nav className="nook-tabs">
          <ContextTabs />
          <span style={{ flex: 1 }} />
          <NavLink
            to="/team"
            className={({ isActive }) => `nook-tab${isActive ? " active" : ""}`}
            title="members, invites and your organizations"
          >
            <Users size={14} />
            Team
          </NavLink>
          {COMING_SOON.map((s) => (
            <span key={s.label} className="nook-tab soon" title="coming soon">
              <s.icon size={14} />
              {s.label}
              <span className="soon-badge">soon</span>
            </span>
          ))}
        </nav>
        <div className="nook-topbar-right">
          <PendingInteractions />
          <NotificationBell />
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
          <button className="terminal-pill" title="open a terminal" onClick={openTerminal}>
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
        {sections.map((s) => (
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
            {s.to === "/chat" && chatUnread > 0 && (
              <span className="rail-badge" aria-label={`${chatUnread} unread`}>
                {chatUnread > 99 ? "99+" : chatUnread}
              </span>
            )}
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
        <span style={{ flex: 1 }} />
        {/* The control plane's real version, not a literal. This read
            "NookOS 0.1.0" from a hardcoded string for every release since
            0.1.0, so the one number on screen claiming to identify the build
            never once did. */}
        <span className="faint" title="control plane version">
          NookOS {cpVersion ?? "…"}
        </span>
      </footer>
    </div>
  );
}
