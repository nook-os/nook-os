// MAIN-321: `/sessions` opens the first session instead of an inventory.
//
// The three things worth pinning are the three ways this can be wrong: it opens
// the wrong session, it replaces nothing so Back breaks, or it decides "you
// have none" before the list has arrived. jsdom only, no control plane.
import React from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter, Route, Routes, useParams } from "react-router-dom";

const SESSIONS = [
  {
    id: "s-first",
    name: "alpha",
    runtime: "bash",
    status: "running",
    workspace_id: null,
    created_by: "u1",
  },
  {
    id: "s-second",
    name: "beta",
    runtime: "claude",
    status: "running",
    workspace_id: null,
    created_by: "u1",
  },
];

const get = vi.hoisted(() =>
  vi.fn(async (path: string) => {
    if (path === "/api/v1/auth/me") return { data: { user: { id: "u1" } } };
    if (path === "/api/v1/workspaces") return { data: [] };
    if (path === "/api/v1/sessions") return { data: (globalThis as any).__sessions };
    return { data: [] };
  }),
);

vi.mock("@nookos/api", () => ({
  api: { GET: get, POST: vi.fn(), PATCH: vi.fn(), DELETE: vi.fn() },
  attachSession: vi.fn(),
}));

import { SessionsIndex } from "./pages/Session";

/// Render `/sessions` with a stand-in for the session route, so "which session
/// did it open" is readable as text rather than inferred from a mock.
function renderAt(sessions: unknown[]) {
  (globalThis as any).__sessions = sessions;
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={["/sessions"]}>
        <Routes>
          <Route path="/sessions" element={<SessionsIndex />} />
          <Route path="/sessions/list" element={<div>THE LIST</div>} />
          <Route path="/sessions/:id" element={<Opened />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

/// Renders WHICH session was opened, so the assertion reads the outcome rather
/// than inferring it from a navigate mock.
function Opened() {
  const { id } = useParams<{ id: string }>();
  return <div>opened:{id}</div>;
}

afterEach(() => {
  cleanup();
  get.mockClear();
});

describe("SessionsIndex", () => {
  it("opens the FIRST session, not the second and not the list", async () => {
    renderAt(SESSIONS);
    expect(await screen.findByText("opened:s-first")).toBeTruthy();
    expect(screen.queryByText("opened:s-second")).toBeNull();
    expect(screen.queryByText("THE LIST")).toBeNull();
  });

  it("shows an empty state, not a redirect, when there are no sessions", async () => {
    renderAt([]);
    expect(await screen.findByText(/No running sessions/)).toBeTruthy();
  });

  it("keeps the list reachable from the empty state", async () => {
    renderAt([]);
    const link = await screen.findByText(/all sessions/);
    expect(link.closest("a")?.getAttribute("href")).toBe("/sessions/list");
  });

  it("does not claim there are no sessions before the list has arrived", async () => {
    // The failure this prevents is a flash of "no running sessions" on every
    // navigation for somebody who has ten. Nothing is decided while pending.
    let release: (v: unknown) => void = () => {};
    const pending = new Promise((r) => {
      release = r;
    });
    get.mockImplementation(async (path: string) => {
      if (path === "/api/v1/auth/me") return { data: { user: { id: "u1" } } };
      if (path === "/api/v1/workspaces") return { data: [] };
      if (path === "/api/v1/sessions") {
        await pending;
        return { data: SESSIONS };
      }
      return { data: [] };
    });

    const { container } = renderAt(SESSIONS);
    expect(container.textContent).not.toContain("No running sessions");
    release(null);
    await waitFor(() => expect(get).toHaveBeenCalled());
  });
});
