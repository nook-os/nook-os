// MAIN-226: Mission Control renders repo → node → checkout → sessions, ghosts
// missing checkouts (not hidden), and offers "+ worktree" on primary clones only.
// jsdom; heavy deps mocked.
import React from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

const OVERVIEW = {
  workspaces: [
    {
      id: "w1",
      name: "acme-api",
      slug: "acme-api",
      git_remote_url: "git@github.com:acme/api.git",
      git_remote_normalized: "github.com/acme/api",
      checkouts: [
        {
          id: "clone1",
          node_id: "n1",
          node_name: "builder-1",
          node_status: "online",
          path: "/srv/acme/api",
          branch: "main",
          kind: "clone",
          dirty: false,
          missing_at: null,
          sessions: [
            { id: "sess1", name: "claude-run", runtime: "claude", status: "running", created_by: "u1" },
          ],
        },
        {
          id: "wt1",
          node_id: "n1",
          node_name: "builder-1",
          node_status: "online",
          path: "/srv/acme/api__feature",
          branch: "feature",
          kind: "worktree",
          dirty: true,
          missing_at: null,
          sessions: [],
        },
        {
          id: "gone1",
          node_id: "n1",
          node_name: "builder-1",
          node_status: "online",
          path: "/srv/acme/api__old",
          branch: "old",
          kind: "worktree",
          dirty: false,
          missing_at: "2026-07-29T00:00:00Z",
          sessions: [],
        },
      ],
      unbound_sessions: [],
    },
  ],
  loose_sessions: [],
};

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string) => {
      if (path === "/api/v1/overview") return { data: OVERVIEW };
      if (path === "/api/v1/auth/me") return { data: { user: { id: "u1" } } };
      return { data: null };
    }),
    POST: vi.fn(async () => ({ data: { id: "newsess" }, response: { ok: true } })),
  },
}));

vi.mock("@nookos/ui", () => ({
  Panel: ({ title, actions, children }: { title: string; actions?: React.ReactNode; children: React.ReactNode }) => (
    <div>
      <div>{title}</div>
      {actions}
      {children}
    </div>
  ),
  Pill: ({ children }: { children: React.ReactNode }) => <span>{children}</span>,
  StatusDot: () => <span />,
  Empty: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
}));

vi.mock("../newwork", () => ({ useNewWork: () => () => {} }));
vi.mock("../sessionOwner", () => ({ SessionOwner: () => <span>owner</span> }));
vi.mock("../dialogs", () => ({ notify: vi.fn() }));

import { MissionPage } from "./pages/Mission";

afterEach(cleanup);

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <MemoryRouter>
      <QueryClientProvider client={qc}>
        <MissionPage />
      </QueryClientProvider>
    </MemoryRouter>,
  );
}

describe("Mission Control (MAIN-226)", () => {
  it("renders checkouts with kind badges and the session under its checkout", async () => {
    renderPage();
    await waitFor(() => expect(screen.getByTestId("checkout-clone1")).toBeTruthy());
    expect(screen.getByTestId("checkout-wt1")).toBeTruthy();
    // The running session appears (under its clone).
    expect(screen.getByTestId("session-sess1")).toBeTruthy();
  });

  it("offers + worktree on the primary clone only", async () => {
    renderPage();
    await waitFor(() => expect(screen.getByTestId("worktree-clone1")).toBeTruthy());
    // Not on a worktree checkout, and not on a missing one.
    expect(screen.queryByTestId("worktree-wt1")).toBeNull();
    expect(screen.queryByTestId("worktree-gone1")).toBeNull();
  });

  it("ghosts a missing checkout but still shows it, and hides its terminal action", async () => {
    renderPage();
    const gone = await screen.findByTestId("checkout-gone1");
    expect(gone.className).toContain("ghost"); // shown, not hidden
    // A present checkout offers "terminal here"; a missing one does not.
    expect(screen.getByTestId("terminal-clone1")).toBeTruthy();
    expect(screen.queryByTestId("terminal-gone1")).toBeNull();
  });
});
