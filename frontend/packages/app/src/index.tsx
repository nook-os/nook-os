import React, { useEffect, useState } from "react";
import { OperatorPage } from "./pages/Operator";
import { installWriteFailureToasts, Toasts } from "./Notifications";
import {
  QueryClient,
  QueryClientProvider,
  useQuery,
} from "@tanstack/react-query";
import { BrowserRouter, Route, Routes, useNavigate } from "react-router-dom";
import { api } from "@nookos/api";
import { Empty, ThemeProvider } from "@nookos/ui";
import { Shell } from "./layout";
import { startLive } from "./live";
import { AcceptInvitePage } from "./pages/AcceptInvite";
import { ActivityPage } from "./pages/Activity";
import { BoardPage } from "./pages/Board";
import { Dashboard } from "./pages/Dashboard";
import { DocsPage } from "./pages/Docs";
import { FeedbackPage } from "./pages/Feedback";
import { Login } from "./pages/Login";
import { Connect } from "./pages/Connect";
import { checkForUpdate, initDesktop, installUpdate, isDesktop, type AvailableUpdate } from "./desktop";
import { installLinkHandler, registerNavigator } from "./links";
import { NodeDetail, NodesPage } from "./pages/Nodes";
import { SessionPage, SessionsPage } from "./pages/Session";
import { SettingsPage } from "./pages/Settings";
import { VerifyEmailPage } from "./pages/VerifyEmail";
import { WorkspaceDetail, WorkspacesPage } from "./pages/Workspaces";

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
  useEffect(() => {
    if (!isDesktop()) return;
    return installLinkHandler((path) => navigate(path));
  }, [navigate]);

  // The desktop build has no control plane on its own origin, so the stored
  // endpoint has to be loaded and applied BEFORE the first request goes out —
  // otherwise /auth/me is sent to tauri://localhost and fails in a way that
  // looks like being signed out.
  const [update, setUpdate] = useState<AvailableUpdate | null>(null);
  const [endpointReady, setEndpointReady] = useState(!isDesktop());
  const [needsConnect, setNeedsConnect] = useState(false);

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
    initDesktop()
      .then((stored) => {
        setNeedsConnect(!stored?.base_url);
        setEndpointReady(true);
      })
      .catch(() => {
        setNeedsConnect(true);
        setEndpointReady(true);
      });
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
  }, [me]);

  if (!endpointReady) return <Empty>Starting…</Empty>;
  if (needsConnect)
    return <Connect onDone={() => { setNeedsConnect(false); refetch(); }} />;
  if (isLoading) return <Empty>Connecting…</Empty>;
  // A desktop client with a rejected token needs its endpoint fixed, not a
  // sign-in form it cannot use — there is no cookie session to establish.
  if (isError || !me) return isDesktop() ? <Connect onDone={() => refetch()} /> : <Login />;

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
        <Route path="workspaces" element={<WorkspacesPage />} />
        <Route path="workspaces/:id" element={<WorkspaceDetail />} />
        <Route path="sessions" element={<SessionsPage />} />
        <Route path="sessions/:id" element={<SessionPage />} />
        <Route path="board" element={<BoardPage />} />
        <Route path="accept" element={<AcceptInvitePage />} />
        <Route path="operator" element={<OperatorPage />} />
        <Route path="activity" element={<ActivityPage />} />
        <Route path="nodes" element={<NodesPage />} />
        <Route path="nodes/:id" element={<NodeDetail />} />
        <Route path="settings" element={<SettingsPage />} />
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
          <AuthGate />
          {/* Outside the router so a toast survives navigation — the thing it
              is telling you about often IS a navigation. */}
          <Toasts />
        </BrowserRouter>
      </ThemeProvider>
    </QueryClientProvider>
  );
}
