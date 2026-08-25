// MAIN-638: the Board page follows the workspace selected in the top bar.
//
// `boards[0]` was the old answer and it was arbitrary the moment a board began
// belonging to a workspace (MAIN-637) — the page showed a repo nobody had
// asked about, while every other page in the app was scoped to the one in the
// switcher. The four things worth pinning are the four ways that is still
// wrong: the wrong board renders, a switch leaves the previous board's cards
// on screen, an empty workspace creates an UNSCOPED board, or the health and
// automation surfaces act on a board other than the one displayed.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";

const BOARDS = [
  // FIRST in the list on purpose: every assertion below that expects `beta`
  // would also pass under `boards[0]` if this were the other way round.
  { id: "b-alpha", workspace_id: "ws-alpha", name: "Alpha", key: "ALPHA", automation: {} },
  { id: "b-beta", workspace_id: "ws-beta", name: "Beta", key: "BETA", automation: {} },
];

// One column of each type the two tabs read, so "the Backlog follows the same
// board" is an assertion about a card and not only about a heading.
const COLUMNS = [
  { id: "c-triage", name: "Triage", position: 0, type: "backlog" },
  { id: "c-todo", name: "Todo", position: 1, type: "unstarted" },
];

const card = (board: (typeof BOARDS)[number], column: string, where: string) => ({
  id: `t-${board.id}-${column}`,
  key: `${board.key}-${column === "c-triage" ? 2 : 1}`,
  title: `${where} card on ${board.name}`,
  column_id: column,
  position: 0,
  type: "task",
  visibility: "team",
  labels: [],
  workspace_id: board.workspace_id,
  priority: 0,
  created_at: "2026-08-01T00:00:00Z",
});

const detailFor = (boardId: string) => {
  const board = BOARDS.find((b) => b.id === boardId)!;
  return {
    board,
    columns: COLUMNS.map((c) => ({ ...c, board_id: boardId })),
    tasks: [card(board, "c-todo", "kanban"), card(board, "c-triage", "backlog")],
  };
};

const get = vi.hoisted(() =>
  vi.fn(async (path: string, opts?: any) => {
    if (path === "/api/v1/boards") return { data: (globalThis as any).__boards };
    if (path === "/api/v1/boards/{id}")
      return { data: detailFor(opts.params.path.id) };
    if (path === "/api/v1/boards/{id}/health")
      return { data: { board_id: opts.params.path.id, checks: [] } };
    if (path === "/api/v1/auth/me")
      return { data: { user: { id: "u1" }, tenant: { id: "tn-1" } } };
    if (path === "/api/v1/workspaces/{id}")
      return { data: { id: opts.params.path.id, name: opts.params.path.id, locations: [] } };
    if (path === "/api/v1/workspaces") return { data: { rows: [], next_cursor: null } };
    if (path === "/api/v1/tenants/{id}/members") return { data: { rows: [] } };
    return { data: [] };
  }),
);
// Typed arguments, not a bare `vi.fn(async () => …)`: the assertions below read
// `post.mock.calls[0][1].body`, and an implementation taking nothing types the
// call tuple as empty.
const post = vi.hoisted(() =>
  vi.fn(async (_path: string, _opts?: { body?: Record<string, unknown> }) => ({
    data: {},
  })),
);

vi.mock("@nookos/api", () => ({
  api: { GET: get, POST: post, PATCH: vi.fn(), PUT: vi.fn(), DELETE: vi.fn() },
  attachSession: vi.fn(),
}));

import { BoardPage, selectedBoard } from "./Board";
import { ContextMenuProvider } from "../contextMenu";
import { useWorkspaceContext } from "../context";

function renderBoard(workspaceId: string | null, boards: unknown[] = BOARDS) {
  (globalThis as any).__boards = boards;
  useWorkspaceContext.setState({ selectedWorkspaceId: workspaceId });
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={["/board"]}>
        <ContextMenuProvider>
          <BoardPage />
        </ContextMenuProvider>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

beforeEach(() => localStorage.clear());
afterEach(cleanup);

describe("which board the page picks (AC-1)", () => {
  it("takes the selected workspace's board, not the first row", () => {
    expect(selectedBoard(BOARDS, "ws-beta")?.id).toBe("b-beta");
  });

  it("has nothing to show when no workspace is selected", () => {
    // Not `boards[0]`: one selector decides (NG-1), and a fallback here is
    // exactly the arbitrary pick this card removes.
    expect(selectedBoard(BOARDS, null)).toBeUndefined();
  });

  it("has nothing to show for a workspace whose board is missing", () => {
    expect(selectedBoard(BOARDS, "ws-gamma")).toBeUndefined();
  });

  it("never falls back to a detached board", () => {
    const detached = [{ id: "b-loose", workspace_id: null, name: "Loose" }];
    expect(selectedBoard(detached, "ws-beta")).toBeUndefined();
  });
});

describe("the Board page", () => {
  it("renders the selected workspace's board and not the other (AC-1)", async () => {
    renderBoard("ws-beta");
    expect(await screen.findByText("kanban card on Beta")).toBeTruthy();
    expect(screen.queryByText("kanban card on Alpha")).toBeNull();
  });

  it("shows the board's key, so which board is on screen is answerable (AC-5)", async () => {
    renderBoard("ws-beta");
    await screen.findByText("kanban card on Beta");
    expect(screen.getByText("BETA")).toBeTruthy();
  });

  it("switches board in place, leaving no card from the previous one (AC-2)", async () => {
    renderBoard("ws-alpha");
    expect(await screen.findByText("kanban card on Alpha")).toBeTruthy();

    // Exactly what the top bar does: the context store changes and the page
    // stays mounted — no remount, no reload. `act` so the store's subscribers
    // re-render inside React's batch rather than after the assertion.
    await act(async () => {
      useWorkspaceContext.setState({ selectedWorkspaceId: "ws-beta" });
    });

    expect(await screen.findByText("kanban card on Beta")).toBeTruthy();
    await waitFor(() =>
      expect(screen.queryByText("kanban card on Alpha")).toBeNull(),
    );
    expect(screen.getByText("BETA")).toBeTruthy();
    expect(screen.queryByText("ALPHA")).toBeNull();
  });

  it("asks a workspace-less viewer to pick one, offering no create button (AC-4)", async () => {
    renderBoard(null);
    expect(
      await screen.findByText("Pick a workspace in the top bar to see its board."),
    ).toBeTruthy();
    // A create button here could only make an unscoped board — the row this
    // page would then never find again.
    expect(screen.queryByText("create one")).toBeNull();
  });

  it("creates a board FOR the selected workspace from the empty state (AC-4)", async () => {
    renderBoard("ws-gamma");
    await userEvent.click(await screen.findByText("create one"));
    await waitFor(() => expect(post).toHaveBeenCalled());
    expect(post.mock.calls[0][0]).toBe("/api/v1/boards");
    expect(post.mock.calls[0][1]?.body?.workspace_id).toBe("ws-gamma");
  });

  it("reads health for the displayed board (AC-6)", async () => {
    renderBoard("ws-beta");
    await screen.findByText("kanban card on Beta");
    await userEvent.click(screen.getByRole("tab", { name: /Health/ }));
    await waitFor(() =>
      expect(
        get.mock.calls.some(
          ([path, opts]) =>
            path === "/api/v1/boards/{id}/health" &&
            (opts as any).params.path.id === "b-beta",
        ),
      ).toBe(true),
    );
    // And never for the board it is not showing.
    expect(
      get.mock.calls.some(
        ([path, opts]) =>
          path === "/api/v1/boards/{id}/health" &&
          (opts as any).params.path.id === "b-alpha",
      ),
    ).toBe(false);
  });

  it("opens automation on the displayed board (AC-6)", async () => {
    const { container } = renderBoard("ws-beta");
    await screen.findByText("kanban card on Beta");
    await userEvent.click(screen.getByTitle("automation"));
    // The dialog names the board it will PATCH; `Beta` is the displayed one.
    // Read off the header element rather than by text: `Automation · {name}` is
    // two text nodes, which no single-node text matcher spans.
    await waitFor(() =>
      expect(container.querySelector(".modal-header")?.textContent).toContain(
        "Automation · Beta",
      ),
    );
  });

  it("shows the same board on the Backlog tab as on the kanban (AC-3)", async () => {
    renderBoard("ws-beta");
    await screen.findByText("kanban card on Beta");
    await userEvent.click(screen.getByRole("tab", { name: /Backlog/ }));
    // The other tab of the SAME board — its backlog card, and its key, with
    // nothing of Alpha's on either.
    expect(await screen.findByText("backlog card on Beta")).toBeTruthy();
    expect(screen.queryByText("backlog card on Alpha")).toBeNull();
    expect(screen.getByText("BETA")).toBeTruthy();
    expect(screen.queryByText("ALPHA")).toBeNull();
  });
});
