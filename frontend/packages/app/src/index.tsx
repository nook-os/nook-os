import React, { useEffect } from "react";
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
  const { data: me, isLoading, isError } = useQuery({
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

  if (isLoading) return <Empty>Connecting…</Empty>;
  if (isError || !me) return <Login />;

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
