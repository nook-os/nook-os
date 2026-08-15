// MAIN-298: "New spec" on a workspace — the one click that replaces knowing a
// task id and typing a `/loop/` URL.
//
// Two things have to be true and neither is obvious from reading the component:
// the draft must land in the right board's BACKLOG carrying the workspace (or it
// is not "a draft ticket for this repo"), and the click must end up on that
// ticket's Loop page (or the click did nothing a PM can use). Both are asserted
// against a mocked control plane, jsdom only.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes, useParams } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { Board } from "@nookos/api";

const WS = "ws-alpha";

const state = vi.hoisted(() => ({
  boards: [] as unknown[],
  workspaces: [] as unknown[],
  /** What `POST /boards/{id}/tasks` answers; `null` makes it fail. */
  created: null as Record<string, unknown> | null,
}));

const post = vi.hoisted(() =>
  vi.fn(async () =>
    state.created
      ? { data: state.created, error: undefined, response: { ok: true, statusText: "OK" } }
      : {
          data: undefined,
          error: { error: "board has no backlog column" },
          response: { ok: false, statusText: "Bad Request" },
        },
  ),
);
// Typed args, so `notify.mock.calls[0][1]` is the body rather than an
// out-of-range index on an empty tuple.
const notify = vi.hoisted(() =>
  vi.fn(async (_title: string, _body: string) => {}),
);

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string) => {
      if (path === "/api/v1/boards") return { data: state.boards };
      if (path === "/api/v1/workspaces")
        return { data: { rows: state.workspaces, next_cursor: null } };
      return { data: null };
    }),
    POST: post,
    DELETE: vi.fn(async () => ({ data: {}, response: { ok: true } })),
  },
}));

// The failure path must not open a real modal and hang the test on a click
// nobody is there to make.
vi.mock("./dialogs", () => ({
  notify,
  askChoice: vi.fn(async () => null),
  askConfirm: vi.fn(async () => false),
  askForm: vi.fn(async () => null),
  askText: vi.fn(async () => null),
}));

import { boardForWorkspace, SPEC_DRAFT_TITLE } from "./newspec";
import { WorkspacesPage } from "./pages/Workspaces";

const board = (id: string, workspace_id: string | null = null): Board =>
  ({ id, name: id, provider: "local", workspace_id }) as unknown as Board;

beforeEach(() => {
  state.boards = [board("b-main")];
  state.workspaces = [{ id: WS, name: "example/api", locations: [] }];
  state.created = { id: "task-uuid", key: "MAIN-77" };
  post.mockClear();
  notify.mockClear();
});
afterEach(cleanup);

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={["/workspaces"]}>
        <Routes>
          <Route path="/workspaces" element={<WorkspacesPage />} />
          {/* Stands in for the Loop page: rendering it IS the proof we landed. */}
          <Route path="/loop/:taskId" element={<LoopStub />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

// `useParams`, not `window.location` — MemoryRouter never touches the latter, so
// reading it would report "/" no matter where the click actually went.
function LoopStub() {
  const { taskId } = useParams<{ taskId: string }>();
  return <div data-testid="loop-page">{taskId}</div>;
}

describe("boardForWorkspace", () => {
  it("prefers a board bound to this workspace", () => {
    const bound = board("b-alpha", WS);
    expect(boardForWorkspace([board("b-main"), bound], WS)).toBe(bound);
  });

  it("falls back to the tenant's first board — one shared board is normal", () => {
    const first = board("b-main");
    expect(boardForWorkspace([first, board("b-other", "ws-beta")], WS)).toBe(first);
  });

  it("is undefined when there is no board at all", () => {
    expect(boardForWorkspace([], WS)).toBeUndefined();
    expect(boardForWorkspace(undefined, WS)).toBeUndefined();
  });
});

describe("New spec from a workspace (MAIN-298)", () => {
  it("is offered on the workspace itself, needing no task id (AC-4)", async () => {
    renderPage();
    expect(await screen.findByText("new spec")).toBeTruthy();
  });

  it("files a backlog draft carrying the workspace, then opens its Loop page", async () => {
    renderPage();
    await userEvent.click(await screen.findByText("new spec"));

    await waitFor(() => expect(post).toHaveBeenCalled());
    const [path, opts] = post.mock.calls[0] as unknown as [
      string,
      { params: { path: { id: string } }; body: Record<string, unknown> },
    ];
    expect(path).toBe("/api/v1/boards/{id}/tasks");
    expect(opts.params.path.id).toBe("b-main");
    // AC-2: the backlog by TYPE (so renaming "Triage" cannot break it), scoped
    // to this workspace, and NOT promoted — a human still applies agent-ready.
    expect(opts.body).toMatchObject({
      title: SPEC_DRAFT_TITLE,
      workspace_id: WS,
      column_type: "backlog",
    });
    expect(opts.body.labels).toBeUndefined();

    // AC-1/AC-3: we are on the new ticket's Loop page, by key.
    const landed = await screen.findByTestId("loop-page");
    expect(landed.textContent).toBe("MAIN-77");
  });

  it("navigates by uuid when the board mints no key", async () => {
    state.created = { id: "task-uuid", key: null };
    renderPage();
    await userEvent.click(await screen.findByText("new spec"));
    const landed = await screen.findByTestId("loop-page");
    expect(landed.textContent).toBe("task-uuid");
  });

  it("says why it failed and stays put rather than opening a dead Loop page", async () => {
    state.created = null;
    renderPage();
    await userEvent.click(await screen.findByText("new spec"));

    await waitFor(() => expect(notify).toHaveBeenCalled());
    expect(String(notify.mock.calls[0][1])).toContain("backlog column");
    expect(screen.queryByTestId("loop-page")).toBeNull();
  });

  it("does not create anything when the tenant has no board", async () => {
    state.boards = [];
    renderPage();
    await userEvent.click(await screen.findByText("new spec"));

    await waitFor(() => expect(notify).toHaveBeenCalled());
    expect(post).not.toHaveBeenCalled();
    expect(screen.queryByTestId("loop-page")).toBeNull();
  });
});
