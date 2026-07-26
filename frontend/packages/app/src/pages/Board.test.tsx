// MAIN-99: BoardPage must call every hook unconditionally. A `useState` placed
// after the `if (!board) return` early return threw "Rendered more hooks than
// during the previous render" the moment the `boards` query resolved
// undefined->data, white-screening the board. This mounts the real BoardPage
// and drives that exact transition, asserting the columns render and no hooks
// error is logged.
import React from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { api } from "@nookos/api";
import { BoardPage } from "./Board";

const BOARD = {
  id: "b1",
  name: "Main",
  key: "MAIN",
  tenant_id: "t1",
  provider: "local",
  created_at: "2026-01-01T00:00:00Z",
};

function mockGet(boards: unknown[]) {
  return vi.spyOn(api, "GET").mockImplementation((async (path: string) => {
    if (path === "/api/v1/boards") return { data: boards };
    if (path === "/api/v1/boards/{id}")
      return {
        data: {
          board: BOARD,
          columns: [{ id: "col1", board_id: "b1", name: "Todo", position: 0, type: "unstarted" }],
          tasks: [],
        },
      };
    if (path === "/api/v1/auth/me") return { data: { user: { id: "me" } } };
    // labels, workspaces, tasks(blocked/filtered)
    return { data: [] };
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
  }) as any);
}

function renderBoard() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={["/board"]}>
        <BoardPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("BoardPage hooks order (MAIN-99)", () => {
  it("renders columns as the boards query resolves undefined->data, with no hooks error (AC-2/AC-3)", async () => {
    const errors: string[] = [];
    const spy = vi.spyOn(console, "error").mockImplementation((...a: unknown[]) => {
      errors.push(a.map(String).join(" "));
    });
    mockGet([BOARD]);

    renderBoard();

    // The column only appears if the render that first passes BOTH early returns
    // (once boards AND detail are present) did not throw the hooks error.
    expect(await screen.findByText(/Todo/)).toBeTruthy();
    expect(errors.join("\n")).not.toMatch(/Rendered more hooks|Rules of Hooks/i);
    spy.mockRestore();
  });

  it("still renders the empty state when there is no board (AC-4)", async () => {
    mockGet([]);
    renderBoard();
    expect(await screen.findByText(/No boards yet/)).toBeTruthy();
  });
});
