// MAIN-226: Mission Control renders the annunciator deck and the repo → node →
// checkout → session tree; ghosts hide behind the toggle, "+ worktree" is
// clone-only, lamps filter, and the live agent mark shows. jsdom; heavy deps
// mocked.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
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
            {
              id: "sess1",
              name: "claude-run",
              runtime: "claude",
              status: "running",
              created_by: "u1",
            },
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
    POST: vi.fn(async () => ({
      data: { id: "newsess" },
      response: { ok: true },
    })),
  },
}));

vi.mock("@nookos/ui", () => ({
  Panel: ({
    title,
    actions,
    children,
  }: {
    title: string;
    actions?: React.ReactNode;
    children: React.ReactNode;
  }) => (
    <div>
      <div>{title}</div>
      {actions}
      {children}
    </div>
  ),
  Pill: ({ children }: { children: React.ReactNode }) => (
    <span>{children}</span>
  ),
  StatusDot: () => <span />,
  Empty: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
  statusTone: () => "ok",
}));

vi.mock("./newwork", () => ({ useNewWork: () => () => {} }));
vi.mock("./sessionOwner", () => ({ SessionOwner: () => <span>owner</span> }));
vi.mock("./dialogs", () => ({ notify: vi.fn() }));

// A controllable stand-in for the live store: tests mutate `liveState` and the
// component reads it through the same selector API.
const liveState = {
  nodeStatus: {} as Record<string, string>,
  sessionStatus: {} as Record<string, string>,
  agentState: {} as Record<
    string,
    { state: string; window: number | null; at: number }
  >,
};
vi.mock("./live", () => ({
  useLive: (sel: (s: typeof liveState) => unknown) => sel(liveState),
  liveAgentMark: (status: string, agent?: { state: string }) =>
    status === "exited" || status === "error" || status === "killed"
      ? undefined
      : agent,
}));

import { MissionPage } from "./pages/Mission";
import { ContextMenuProvider } from "./contextMenu";

beforeEach(() => {
  liveState.nodeStatus = {};
  liveState.sessionStatus = {};
  liveState.agentState = {};
  window.localStorage.clear();
});
afterEach(cleanup);

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <MemoryRouter>
      <QueryClientProvider client={qc}>
        <ContextMenuProvider>
          <MissionPage />
        </ContextMenuProvider>
      </QueryClientProvider>
    </MemoryRouter>,
  );
}

describe("Mission Control (MAIN-226)", () => {
  it("renders the deck stats and the checkout tree with the session under its clone", async () => {
    renderPage();
    await waitFor(() =>
      expect(screen.getByTestId("checkout-clone1")).toBeTruthy(),
    );
    expect(screen.getByTestId("checkout-wt1")).toBeTruthy();
    expect(screen.getByTestId("session-sess1")).toBeTruthy();
    expect(screen.getByTestId("deck").textContent).toContain("1/1 node");
    expect(screen.getByTestId("deck").textContent).toContain("3 checkouts");
  });

  it("the row \u22ef menu offers worktree on the clone", async () => {
    renderPage();
    await waitFor(() =>
      expect(screen.getByTestId("rowmenu-clone1")).toBeTruthy(),
    );
    fireEvent.click(screen.getByTestId("rowmenu-clone1"));
    expect(screen.getByText("Terminal here")).toBeTruthy();
    expect(screen.getByText("New worktree from this clone")).toBeTruthy();
    expect(screen.getByText("Copy path")).toBeTruthy();
  });

  it("the row \u22ef menu on a worktree has no worktree item", async () => {
    renderPage();
    await waitFor(() => expect(screen.getByTestId("rowmenu-wt1")).toBeTruthy());
    fireEvent.click(screen.getByTestId("rowmenu-wt1"));
    expect(screen.getByText("Terminal here")).toBeTruthy();
    expect(screen.queryByText("New worktree from this clone")).toBeNull();
  });

  it("switches between tree, grid, machines and matrix views", async () => {
    renderPage();
    await waitFor(() =>
      expect(screen.getByTestId("checkout-clone1")).toBeTruthy(),
    );

    fireEvent.click(screen.getByTestId("view-grid"));
    expect(screen.getByTestId("grid-view")).toBeTruthy();
    expect(screen.getByTestId("card-acme-api")).toBeTruthy();

    fireEvent.click(screen.getByTestId("view-machines"));
    expect(screen.getByTestId("machines-view")).toBeTruthy();
    expect(screen.getByTestId("machine-n1")).toBeTruthy();

    fireEvent.click(screen.getByTestId("view-matrix"));
    expect(screen.getByTestId("matrix-view")).toBeTruthy();

    fireEvent.click(screen.getByTestId("view-canvas"));
    expect(screen.getByTestId("canvas-view")).toBeTruthy();
    expect(screen.getByTestId("canvas-n1")).toBeTruthy();

    fireEvent.click(screen.getByTestId("view-tree"));
    expect(screen.getByTestId("repo-acme-api")).toBeTruthy();
    // The chosen view persists.
    expect(window.localStorage.getItem("nook.mission.view.v1")).toBe("tree");
  });

  it("hides ghosts by default; the toggle reveals them ghosted, not hidden", async () => {
    renderPage();
    await waitFor(() =>
      expect(screen.getByTestId("checkout-clone1")).toBeTruthy(),
    );
    // Hidden by default, with a per-repo hint.
    expect(screen.queryByTestId("checkout-gone1")).toBeNull();
    expect(screen.getByTestId("ghosts-acme-api")).toBeTruthy();

    fireEvent.click(screen.getByTestId("ghost-toggle"));
    const gone = await screen.findByTestId("checkout-gone1");
    expect(gone.className).toContain("ghost"); // shown, ghosted
  });

  it("lamps light for exceptions and filter the tree when clicked", async () => {
    renderPage();
    await waitFor(() => expect(screen.getByTestId("lamp-dirty")).toBeTruthy());
    expect(screen.getByTestId("lamp-missing")).toBeTruthy();
    expect(screen.queryByTestId("lamp-offline")).toBeNull(); // all nodes online

    fireEvent.click(screen.getByTestId("lamp-dirty"));
    // Only the dirty worktree remains.
    expect(screen.getByTestId("checkout-wt1")).toBeTruthy();
    expect(screen.queryByTestId("checkout-clone1")).toBeNull();

    fireEvent.click(screen.getByTestId("lamp-dirty")); // click again clears
    await waitFor(() =>
      expect(screen.getByTestId("checkout-clone1")).toBeTruthy(),
    );
  });

  it("shows the live agent mark on the session row and a deck chip", async () => {
    liveState.agentState = { sess1: { state: "running", window: null, at: 0 } };
    renderPage();
    await waitFor(() => expect(screen.getByTestId("agent-sess1")).toBeTruthy());
    expect(screen.getByTestId("chip-sess1")).toBeTruthy();
  });
});
