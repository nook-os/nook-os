// MAIN-502 AC-4/AC-7: which view the session page renders.
//
// One fork, two directions, and the SECOND is the one that has to be pinned: a
// terminal session must be provably unchanged — same `TerminalView`, same
// attach, same tmux chrome. A change here does not fail loudly; it renders the
// wrong thing quietly, which is exactly what a test is for. jsdom only, no
// control plane.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter, Route, Routes } from "react-router-dom";

const SESSION_ID = "sess-1";

const state = vi.hoisted(() => ({ interface: "terminal" as "terminal" | "chat" }));
const attach = vi.hoisted(() => vi.fn(() => () => {}));

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string) => {
      if (path === "/api/v1/sessions/{id}")
        return {
          data: {
            id: SESSION_ID,
            name: "dogfood",
            runtime: "claude",
            status: "running",
            workspace_id: null,
            node_id: "node-1",
            node_online: true,
            created_by: "u1",
            interface: state.interface,
          },
        };
      if (path === "/api/v1/sessions/{id}/messages")
        return { data: [{
          id: "m-1",
          session_id: SESSION_ID,
          role: "agent",
          body: "a line of the conversation",
          permission_request_id: null,
          tool_name: null,
          decision: null,
          at: "2026-08-10T10:00:00Z",
        }] };
      if (path === "/api/v1/sessions") return { data: [] };
      if (path === "/api/v1/nodes") return { data: [{ id: "node-1", name: "box" }] };
      return { data: [] };
    }),
    POST: vi.fn(async () => ({ data: {} })),
    PUT: vi.fn(async () => ({ data: {} })),
    PATCH: vi.fn(async () => ({ data: {} })),
    DELETE: vi.fn(async () => ({ data: {} })),
  },
  attachSession: attach,
}));

// The terminal is an xterm.js canvas — nothing this file asserts on, and it
// does not render under jsdom. Standing in for it lets the FORK be tested
// without dragging a renderer into the test.
vi.mock("@nookos/ui", async () => {
  const actual = await vi.importActual<Record<string, unknown>>("@nookos/ui");
  return {
    ...actual,
    TerminalView: (props: { attach: (h: unknown) => void }) => {
      props.attach({});
      return <div data-testid="terminal-view" />;
    },
  };
});

import { ContextMenuProvider } from "../contextMenu";
import { SessionPage } from "./Session";

function renderSession() {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={[`/sessions/${SESSION_ID}`]}>
        {/* The session chrome (tabs, the terminal's right-click region) reads
            this context; without it the page throws before the fork renders. */}
        <ContextMenuProvider>
          <Routes>
            <Route path="/sessions/:id" element={<SessionPage />} />
          </Routes>
        </ContextMenuProvider>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  state.interface = "terminal";
  attach.mockClear();
});
afterEach(cleanup);

describe("the session view", () => {
  // AC-7. The default, and the one that must not move.
  it("renders the terminal — and attaches to it — for a terminal session", async () => {
    renderSession();
    expect(await screen.findByTestId("terminal-view")).toBeTruthy();
    expect(screen.queryByTestId("session-chat")).toBeNull();
    await waitFor(() => expect(attach).toHaveBeenCalled());
  });

  // AC-4.
  it("renders the conversation for a chat session", async () => {
    state.interface = "chat";
    renderSession();
    expect(await screen.findByTestId("session-chat")).toBeTruthy();
    expect(await screen.findByText("a line of the conversation")).toBeTruthy();
    expect(screen.queryByTestId("terminal-view")).toBeNull();
    // …and nothing attached a PTY. A chat session has none, so an attach here
    // would be a socket retrying forever against a session that will never
    // stream (MAIN-502 AC-3).
    expect(attach).not.toHaveBeenCalled();
  });
});
