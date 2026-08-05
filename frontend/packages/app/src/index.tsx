import React, { useEffect, useState } from "react";
import { AdminPage } from "./pages/Admin";
import { installWriteFailureToasts, Toasts } from "./Notifications";
import {
  QueryClient,
  QueryClientProvider,
  useQuery,
} from "@tanstack/react-query";
import { BrowserRouter, Navigate, Route, Routes, useLocation, useNavigate } from "react-router-dom";
import { api } from "@nookos/api";
import { Empty, ThemeProvider } from "@nookos/ui";
import { ContextMenuProvider } from "./contextMenu";
import { Shell } from "./layout";
import { startLive } from "./live";
import { AcceptInvitePage } from "./pages/AcceptInvite";
import { BoardPage } from "./pages/Board";
import { ChatPage } from "./pages/Chat";
import { Dashboard } from "./pages/Dashboard";
import { DocsPage } from "./pages/Docs";
import { FeedbackPage } from "./pages/Feedback";
import { Login } from "./pages/Login";
import { Connect } from "./pages/Connect";
import { awaitLocalStack, checkForUpdate, initDesktop, installUpdate, isDesktop, setControlPlaneAccount, type AvailableUpdate } from "./desktop";
import { installLinkHandler, registerNavigator } from "./links";
import { NodeDetail, NodesPage } from "./pages/Nodes";
import { Notebook } from "./pages/Notebook";
import { SessionPage, SessionsIndex, SessionsPage } from "./pages/Session";
import { SettingsPage } from "./pages/Settings";
import { TeamPage } from "./pages/Team";
import { VerifyEmailPage } from "./pages/VerifyEmail";
import { WorkspaceDetail, WorkspacesPage } from "./pages/Workspaces";
import { MissionPage } from "./pages/Mission";
import { LoopPage } from "./pages/Loop";

/** /operator carried ?section= deep links before it merged into /admin;
 *  preserve them rather than dumping everyone on the first section. */
/** The bundled control plane failed to start, so say why (MAIN-396 AC-3).
 *
 *  The log is rendered verbatim and monospaced: it is the control plane's own
 *  output, and paraphrasing a migration failure helps nobody. */
export function LocalStackFailed({ log }: { log: string }) {
  return (
    <div style={{ padding: "2rem", fontFamily: "monospace", fontSize: 13 }}>
      <h1 style={{ fontSize: 15, marginBottom: "0.75rem" }}>
        The local control plane did not start.
      </h1>
      <pre
        data-testid="local-stack-log"
        style={{ whiteSpace: "pre-wrap", overflowX: "auto", opacity: 0.85 }}
      >
        {log}
      </pre>
    </div>
  );
}

function LegacyOperatorRedirect() {
  const location = useLocation();
  return <Navigate to={`/admin${location.search}`} replace />;
}

const queryClient = new QueryClient({
  defaultOptions: {
    queries: { staleTime: 5000, retry: 1, refetchOnWindowFocus: false },
  },
});

function AuthGate() {
  // Before anything else, and outside the signed-in branch: the connect screen
  // shows a link too, and a link that navigates this webview is what broke
  // sign-in there in the first place.
  const navigate = useNavigate();
  // Both builds: a clicked desktop notification has to reach the router from
  // outside React, and that is not a desktop-only need.
  useEffect(() => registerNavigator((path) => navigate(path)), [navigate]);
  // Above the auth gate on purpose: a write that fails before you are through
  // it — signing in with a password, say — is exactly as silent otherwise.
  useEffect(() => installWriteFailureToasts(), []);
  // Both builds intercept links, differing only in what a link that is NOT ours
  // should do (MAIN-279). Desktop must keep its single webview on the app, so an
  // external link goes to the OS browser. The web build leaves external links
  // entirely alone — there is a URL bar and a tab to open them in — and only
  // claims its own, so a raw `<a href>` into the app stops reloading the
  // document and wiping the state the user was looking at.
  useEffect(
    () =>
      installLinkHandler(
        (path) => navigate(path),
        isDesktop() ? "os-browser" : "browser-default",
      ),
    [navigate],
  );

  // The desktop build has no control plane on its own origin, so the stored
  // endpoint has to be loaded and applied BEFORE the first request goes out —
  // otherwise /auth/me is sent to tauri://localhost and fails in a way that
  // looks like being signed out.
  const [update, setUpdate] = useState<AvailableUpdate | null>(null);
  const [endpointReady, setEndpointReady] = useState(!isDesktop());
  const [needsConnect, setNeedsConnect] = useState(false);
  /** The bundled control plane's own log, when it never became healthy. */
  const [localStackError, setLocalStackError] = useState<string | null>(null);
  // The active server's URL (desktop only) — prefills the Connect screen when a
  // token expires (AC-6) and names the entry whose account we backfill (AC-1).
  const [activeUrl, setActiveUrl] = useState<string>("");

  // Checked once at startup and then hourly. Offered, never forced: an app
  // that restarted itself the moment a release appeared would do it in the
  // middle of whatever you were reading.
  useEffect(() => {
    if (!isDesktop()) return;
    const check = () => checkForUpdate().then(setUpdate);
    check();
    const t = setInterval(check, 60 * 60 * 1000);
    return () => clearInterval(t);
  }, []);

  useEffect(() => {
    if (!isDesktop()) return;
    let cancelled = false;
    void (async () => {
      // Wait on the SHELL, not on a connection error (MAIN-396 AC-3). It holds
      // the child's output, so a control plane that died during migration
      // arrives with its reason attached — and the window stays on "Starting…"
      // meanwhile instead of rendering an app pointed at a dead port.
      //
      // `null` here is not a failure: off the desktop, or on a shell that
      // predates the command, there simply is no local stack to wait for.
      const stack = await awaitLocalStack();
      if (cancelled) return;
      setLocalStackError(stack?.error ?? null);
      try {
        const stored = await initDesktop();
        if (cancelled) return;
        setActiveUrl(stored?.base_url ?? "");
        setNeedsConnect(!stored?.base_url);
      } catch {
        if (!cancelled) setNeedsConnect(true);
      }
      if (!cancelled) setEndpointReady(true);
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const { data: me, isLoading, isError, refetch } = useQuery({
    enabled: endpointReady && !needsConnect,
    queryKey: ["me"],
    queryFn: async () => {
      const { data, response } = await api.GET("/api/v1/auth/me");
      if (response.status === 401) return null;
      return data ?? null;
    },
    retry: false,
  });

  useEffect(() => {
    if (me) startLive(queryClient);
    // Record which account is signed in on the active server, so the switcher
    // can show it on that row without switching to it (AC-1).
    if (me && isDesktop() && activeUrl) {
      void setControlPlaneAccount(activeUrl, me.user.email);
    }
  }, [me, activeUrl]);

  if (!endpointReady) return <Empty>Starting…</Empty>;
  // The bundled control plane died AND there is no other one stored, so this
  // window has nothing to talk to. Show the child's log — that is AC-3's whole
  // bar, and the alternative is the blank window it exists to prevent. A user
  // who HAS a stored control plane is not trapped here: they fall through to it.
  if (localStackError && needsConnect) return <LocalStackFailed log={localStackError} />;
  if (needsConnect)
    return <Connect onDone={() => { setNeedsConnect(false); refetch(); }} />;
  if (isLoading) return <Empty>Connecting…</Empty>;
  // A desktop client with a rejected token needs its endpoint fixed, not a
  // sign-in form it cannot use — there is no cookie session to establish. The
  // server's URL is prefilled and the reason named, so a successful sign-in
  // replaces that entry's token and continues into the app (AC-6).
  if (isError || !me)
    return isDesktop() ? (
      <Connect
        prefillUrl={activeUrl}
        notice="Your sign-in for this control plane expired. Sign in again to continue."
        onDone={() => refetch()}
      />
    ) : (
      // Signed out in the browser. The invite landing renders WITHOUT auth so an
      // invitee sees who invited them and a sign-in that carries the token back
      // (MAIN-97); every other path still lands on the login screen.
      <Routes>
        <Route path="/accept" element={<AcceptInvitePage />} />
        <Route path="*" element={<Login />} />
      </Routes>
    );

  return (
    <>
      {update && (
        <div className="update-bar" role="status">
          NookOS {update.version} is available — you are on {update.current}.
          <button className="btn small primary" onClick={() => installUpdate()}>
            update and restart
          </button>
          <button className="btn small" onClick={() => setUpdate(null)}>
            later
          </button>
        </div>
      )}
    <Routes>
      <Route element={<Shell me={me} />}>
        <Route index element={<Dashboard />} />
        <Route path="mission" element={<MissionPage />} />
        <Route path="workspaces" element={<WorkspacesPage />} />
        <Route path="workspaces/:id" element={<WorkspaceDetail />} />
        <Route path="sessions" element={<SessionsIndex />} />
        <Route path="sessions/list" element={<SessionsPage />} />
        <Route path="sessions/:id" element={<SessionPage />} />
        <Route path="board" element={<BoardPage />} />
        {/* The Loop workspace (MAIN-233): one ticket's run, full screen. */}
        <Route path="loop/:taskId" element={<LoopPage />} />
        <Route path="chat" element={<ChatPage />} />
        <Route path="accept" element={<AcceptInvitePage />} />
        <Route path="admin" element={<AdminPage />} />
        {/* The old rail entries live on as redirects: bookmarks and muscle
            memory keep working, and /operator?section=tenants still lands on
            the same table it always named. */}
        <Route path="operator" element={<LegacyOperatorRedirect />} />
        <Route path="activity" element={<Navigate to="/admin?section=activity" replace />} />
        <Route path="nodes" element={<NodesPage />} />
        <Route path="nodes/:id" element={<NodeDetail />} />
        <Route path="notebook" element={<Notebook />} />
        <Route path="settings" element={<SettingsPage />} />
        <Route path="team" element={<TeamPage />} />
        <Route path="verify-email" element={<VerifyEmailPage />} />
        <Route path="feedback" element={<FeedbackPage />} />
        <Route path="help" element={<DocsPage />} />
        <Route path="*" element={<Empty>Nothing here.</Empty>} />
      </Route>
    </Routes>
    </>
  );
}

export function NookApp() {
  return (
    <QueryClientProvider client={queryClient}>
      <ThemeProvider>
        <BrowserRouter>
          {/* One capture-phase contextmenu listener for the whole app: the
              native menu never renders, a Nook menu always does (MAIN-167). */}
          <ContextMenuProvider>
            <AuthGate />
            {/* Outside the router so a toast survives navigation — the thing it
                is telling you about often IS a navigation. */}
            <Toasts />
          </ContextMenuProvider>
        </BrowserRouter>
      </ThemeProvider>
    </QueryClientProvider>
  );
}
