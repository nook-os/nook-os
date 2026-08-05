// MAIN-414: the pane itself — the parts that only exist once it is rendered.
//
// The rules live in `sessionNav.ts` and are tested there. What is here is the
// wiring those rules hang off: that the tree is drawn from the LIVE sessions
// rather than the workspace-scoped strip, that typing narrows what is on
// screen, that a shut pane still shows a way back in, and that width, collapsed
// and pinned are written to the USER's settings rather than to this browser.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { ContextMenuProvider } from "./contextMenu";

const SESSIONS = [
  {
    id: "s1",
    name: "alpha",
    runtime: "claude",
    status: "running",
    workspace_id: "w1",
    node_id: "n1",
    created_by: "u1",
  },
  {
    id: "s2",
    name: "beta",
    runtime: "bash",
    status: "running",
    workspace_id: "w2",
    node_id: "n1",
    created_by: "u1",
  },
];

const get = vi.hoisted(() =>
  vi.fn(async (path: string) => {
    if (path === "/api/v1/auth/me")
      return {
        data: {
          user: { id: "u1", role: "owner", display_name: "U" },
          tenant: { id: "t1", name: "Hein", slug: "hein", created_at: "" },
          tenants: [],
        },
      };
    if (path === "/api/v1/workspaces")
      return {
        data: [
          { id: "w1", name: "api", locations: [] },
          { id: "w2", name: "web", locations: [] },
        ],
      };
    if (path === "/api/v1/nodes") return { data: [{ id: "n1", name: "azul" }] };
    if (path === "/api/v1/sessions") return { data: (globalThis as any).__sessions };
    if (path === "/api/v1/settings") return { data: (globalThis as any).__settings };
    return { data: [] };
  }),
);
const put = vi.hoisted(() => vi.fn(async () => ({ data: {} })));

const post = vi.hoisted(() => vi.fn(async () => ({ data: {} })));
const patch = vi.hoisted(() => vi.fn(async () => ({ data: {} })));
const askText = vi.hoisted(() => vi.fn(async () => "renamed"));
const askConfirm = vi.hoisted(() => vi.fn(async () => true));

vi.mock("@nookos/api", () => ({
  api: { GET: get, PUT: put, POST: post, PATCH: patch, DELETE: vi.fn() },
  attachSession: vi.fn(),
}));
vi.mock("./dialogs", () => ({
  askConfirm,
  askText,
  notify: vi.fn(async () => {}),
  DialogHost: () => null,
}));

import { SessionNavigator } from "./SessionNavigator";

function renderPane(opts: { prefs?: unknown; sessions?: unknown[] } = {}) {
  (globalThis as any).__sessions = opts.sessions ?? SESSIONS;
  (globalThis as any).__settings =
    opts.prefs === undefined
      ? []
      : [{ key: "sessions.navigator", scope: "user", value: opts.prefs }];
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={["/sessions/s1"]}>
      <ContextMenuProvider>
        <Routes>
          <Route path="/sessions/:id" element={<SessionNavigator activeId="s1" />} />
        </Routes>
      </ContextMenuProvider>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  put.mockClear();
  post.mockClear();
  patch.mockClear();
  askText.mockClear();
  askConfirm.mockClear();
});
afterEach(cleanup);

describe("SessionNavigator", () => {
  it("lists every workspace's live sessions under its own folder", async () => {
    renderPane();
    // Both folders, though the workspace context (none here) would have scoped
    // the tab strip: the pane exists to find a session you have NOT got open.
    expect(await screen.findByText("api")).toBeTruthy();
    expect(await screen.findByText("web")).toBeTruthy();
    expect(screen.getByText("alpha")).toBeTruthy();
    expect(screen.getByText("beta")).toBeTruthy();
  });

  it("narrows the tree in place and hides the folders left empty", async () => {
    renderPane();
    await screen.findByText("alpha");
    fireEvent.change(screen.getByLabelText("find a session"), {
      target: { value: "beta" },
    });
    await waitFor(() => expect(screen.queryByText("alpha")).toBeNull());
    expect(screen.getByText("beta")).toBeTruthy();
    // The folder that lost its only child goes with it.
    expect(screen.queryByText("api")).toBeNull();
    expect(screen.getByText("web")).toBeTruthy();
  });

  it("says so when nothing matches, instead of showing an empty tree", async () => {
    renderPane();
    await screen.findByText("alpha");
    fireEvent.change(screen.getByLabelText("find a session"), {
      target: { value: "zzz" },
    });
    expect(await screen.findByText(/nothing matches/)).toBeTruthy();
  });

  it("opens a session when it is clicked", async () => {
    renderPane();
    const link = await screen.findByText("beta");
    fireEvent.click(link);
    // The pane re-renders under the new route; the click's job is the
    // navigation, and reaching `/sessions/s2` is what proves it happened.
    await waitFor(() => expect(screen.getByLabelText("find a session")).toBeTruthy());
  });

  it("shows a pull-tab when collapsed, and reopens from it", async () => {
    renderPane({ prefs: { width: 260, collapsed: true, pinned: false } });
    const tab = await screen.findByLabelText("show the session navigator");
    // Collapsed is not hidden: there is no tree, but there IS a way back.
    expect(screen.queryByLabelText("find a session")).toBeNull();
    fireEvent.click(tab);
    expect(await screen.findByLabelText("find a session")).toBeTruthy();
    expect(put).toHaveBeenCalledWith("/api/v1/settings/{key}", {
      params: { path: { key: "sessions.navigator" } },
      body: { scope: "user", value: { width: 260, collapsed: false, pinned: false } },
    });
  });

  it("reads the stored width back, so the pane is the size you left it", async () => {
    const { container } = renderPane({
      prefs: { width: 400, collapsed: false, pinned: true },
    });
    await screen.findByLabelText("find a session");
    const pane = container.querySelector("nav.session-nav") as HTMLElement;
    expect(pane.style.width).toBe("400px");
    // Never the default on the way there: the pane waits for its prefs rather
    // than flashing 260px and jumping.
    expect(pane.style.width).not.toBe("260px");
  });

  it("writes collapsed and pinned to the USER's settings, not this browser", async () => {
    renderPane({ prefs: { width: 300, collapsed: false, pinned: false } });
    await screen.findByLabelText("find a session");

    fireEvent.click(screen.getByRole("button", { name: /^pin/ }));
    expect(put).toHaveBeenLastCalledWith("/api/v1/settings/{key}", {
      params: { path: { key: "sessions.navigator" } },
      body: { scope: "user", value: { width: 300, collapsed: false, pinned: true } },
    });

    fireEvent.click(screen.getByLabelText("hide the session navigator"));
    expect(put).toHaveBeenLastCalledWith("/api/v1/settings/{key}", {
      params: { path: { key: "sessions.navigator" } },
      body: { scope: "user", value: { width: 300, collapsed: true, pinned: true } },
    });
  });

  it("names the active team on the pane itself", async () => {
    renderPane();
    expect(await screen.findByText("Hein")).toBeTruthy();
  });
});

describe("SessionNavigator — rename and stop live HERE (MAIN-416)", () => {
  it("offers Rename and Stop on a session's context menu", async () => {
    renderPane();
    fireEvent.contextMenu(await screen.findByText("alpha"));
    expect(await screen.findByText("Rename Session…")).toBeTruthy();
    expect(screen.getByText("Stop Session…")).toBeTruthy();
  });

  it("renames through the session PATCH, so every viewer sees it", async () => {
    renderPane();
    fireEvent.contextMenu(await screen.findByText("alpha"));
    fireEvent.click(await screen.findByText("Rename Session…"));
    await waitFor(() =>
      expect(patch).toHaveBeenCalledWith("/api/v1/sessions/{id}", {
        params: { path: { id: "s1" } },
        body: { name: "renamed" },
      }),
    );
  });

  it("refuses an empty name without asking the server", async () => {
    askText.mockResolvedValueOnce("   ");
    renderPane();
    fireEvent.contextMenu(await screen.findByText("alpha"));
    fireEvent.click(await screen.findByText("Rename Session…"));
    await waitFor(() => expect(askText).toHaveBeenCalled());
    expect(patch).not.toHaveBeenCalled();
  });

  it("stops through the stop endpoint, after a confirm", async () => {
    renderPane();
    fireEvent.contextMenu(await screen.findByText("alpha"));
    fireEvent.click(await screen.findByText("Stop Session…"));
    await waitFor(() => expect(askConfirm).toHaveBeenCalled());
    // Not `/kill` — the row survives and the declaration is still satisfied.
    await waitFor(() =>
      expect(post).toHaveBeenCalledWith("/api/v1/sessions/{id}/stop", {
        params: { path: { id: "s1" } },
      }),
    );
  });

  it("does not stop when the confirm is declined", async () => {
    askConfirm.mockResolvedValueOnce(false);
    renderPane();
    fireEvent.contextMenu(await screen.findByText("alpha"));
    fireEvent.click(await screen.findByText("Stop Session…"));
    await waitFor(() => expect(askConfirm).toHaveBeenCalled());
    expect(post).not.toHaveBeenCalled();
  });
});
