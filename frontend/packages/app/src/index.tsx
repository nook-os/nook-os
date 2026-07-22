import React, { useEffect, useState } from "react";
import {
  QueryClient,
  QueryClientProvider,
  useQuery,
} from "@tanstack/react-query";
import { BrowserRouter, Route, Routes } from "react-router-dom";
import { api } from "@nookos/api";
import { Empty, ThemeProvider } from "@nookos/ui";
import { Shell } from "./layout";
import { startLive } from "./live";
import { ActivityPage } from "./pages/Activity";
import { BoardPage } from "./pages/Board";
import { Dashboard } from "./pages/Dashboard";
import { DocsPage } from "./pages/Docs";
import { FeedbackPage } from "./pages/Feedback";
import { Login } from "./pages/Login";
import { Connect } from "./pages/Connect";
import { initDesktop, isDesktop } from "./desktop";
import { NodeDetail, NodesPage } from "./pages/Nodes";
import { SessionPage, SessionsPage } from "./pages/Session";
import { SettingsPage } from "./pages/Settings";
import { WorkspaceDetail, WorkspacesPage } from "./pages/Workspaces";

const queryClient = new QueryClient({
  defaultOptions: {
    queries: { staleTime: 5000, retry: 1, refetchOnWindowFocus: false },
  },
});

function AuthGate() {
  // The desktop build has no control plane on its own origin, so the stored
  // endpoint has to be loaded and applied BEFORE the first request goes out —
  // otherwise /auth/me is sent to tauri://localhost and fails in a way that
  // looks like being signed out.
  const [endpointReady, setEndpointReady] = useState(!isDesktop());
  const [needsConnect, setNeedsConnect] = useState(false);

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
    <Routes>
      <Route element={<Shell me={me} />}>
        <Route index element={<Dashboard />} />
        <Route path="workspaces" element={<WorkspacesPage />} />
        <Route path="workspaces/:id" element={<WorkspaceDetail />} />
        <Route path="sessions" element={<SessionsPage />} />
        <Route path="sessions/:id" element={<SessionPage />} />
        <Route path="board" element={<BoardPage />} />
        <Route path="activity" element={<ActivityPage />} />
        <Route path="nodes" element={<NodesPage />} />
        <Route path="nodes/:id" element={<NodeDetail />} />
        <Route path="settings" element={<SettingsPage />} />
        <Route path="feedback" element={<FeedbackPage />} />
        <Route path="help" element={<DocsPage />} />
        <Route path="*" element={<Empty>Nothing here.</Empty>} />
      </Route>
    </Routes>
  );
}

export function NookApp() {
  return (
    <QueryClientProvider client={queryClient}>
      <ThemeProvider>
        <BrowserRouter>
          <AuthGate />
        </BrowserRouter>
      </ThemeProvider>
    </QueryClientProvider>
  );
}
