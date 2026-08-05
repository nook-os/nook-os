// MAIN-417 AC-2 / NG-1, in the browser: closing a tab ends nothing and asks
// nothing.
//
// The pure rules live in `workingSet.test.ts`. What can only be checked here is
// that the ✕ is wired to the SET and not to the session — the old control
// killed a terminal, and "close" quietly meaning "kill" is the failure this
// card exists to remove. So this asserts the negative directly: no confirm
// dialog, and no POST of any kind.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { ContextMenuProvider } from "./contextMenu";

const SESSIONS = [
  { id: "s1", name: "alpha", runtime: "bash", status: "running", node_id: "n1" },
  { id: "s2", name: "beta", runtime: "claude", status: "exited", node_id: "n1" },
];

const get = vi.hoisted(() =>
  vi.fn(async (path: string) => {
    if (path === "/api/v1/sessions") return { data: SESSIONS };
    if (path === "/api/v1/settings")
      return {
        data: [
          {
            key: "sessions.workingset",
            scope: "user",
            value: { open: ["s1", "s2"], pinned: [], order: [] },
          },
        ],
      };
    if (path === "/api/v1/nodes") return { data: [{ id: "n1", name: "azul" }] };
    return { data: [] };
  }),
);
const put = vi.hoisted(() => vi.fn(async () => ({ data: {} })));
const post = vi.hoisted(() => vi.fn(async () => ({ data: {} })));
const askConfirm = vi.hoisted(() => vi.fn(async () => true));

vi.mock("@nookos/api", () => ({
  api: { GET: get, PUT: put, POST: post, PATCH: vi.fn(), DELETE: vi.fn() },
  attachSession: vi.fn(),
}));
vi.mock("./dialogs", () => ({
  askConfirm,
  askText: vi.fn(async () => null),
  notify: vi.fn(async () => {}),
  DialogHost: () => null,
}));

import { SessionTabs } from "./SessionTabs";

function renderStrip() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={["/sessions/s1"]}>
        <ContextMenuProvider>
        <Routes>
          <Route path="/sessions/:id" element={<SessionTabs activeId="s1" />} />
          <Route path="/sessions" element={<div>THE INDEX</div>} />
        </Routes>
        </ContextMenuProvider>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  put.mockClear();
  post.mockClear();
  askConfirm.mockClear();
});
afterEach(cleanup);

describe("SessionTabs — the strip is the working set", () => {
  it("shows a tab for every open session, including one that exited", async () => {
    // AC-4: `beta` is `exited` and keeps its tab. A strip derived from the live
    // sessions could not show it at all, which is how you lost the ability to
    // restart it in place.
    renderStrip();
    expect(await screen.findByText("alpha")).toBeTruthy();
    expect(screen.getByText("beta")).toBeTruthy();
  });

  it("closing a tab asks NOTHING and ends NOTHING", async () => {
    renderStrip();
    await screen.findByText("beta");

    fireEvent.click(screen.getByLabelText("close beta"));

    // NG-1, asserted as the absence it is: no confirm, and no POST — not a
    // kill, not a restart, not a scale-down.
    expect(askConfirm).not.toHaveBeenCalled();
    expect(post).not.toHaveBeenCalled();

    // It left the set, and that is written where the next machine will read it.
    await waitFor(() =>
      expect(put).toHaveBeenCalledWith("/api/v1/settings/{key}", {
        params: { path: { key: "sessions.workingset" } },
        body: { scope: "user", value: { open: ["s1"], pinned: [], order: [] } },
      }),
    );
    await waitFor(() => expect(screen.queryByText("beta")).toBeNull());
    expect(screen.getByText("alpha")).toBeTruthy();
  });

  it("closing the tab you are looking at moves you off it", async () => {
    renderStrip();
    await screen.findByText("alpha");
    fireEvent.click(screen.getByLabelText("close alpha"));
    // Otherwise the terminal below stays attached to a session with no tab.
    expect(await screen.findByText("THE INDEX")).toBeTruthy();
  });
});

describe("SessionTabs — Stop is NOT reachable from a tab (MAIN-416 NG-2)", () => {
  it("offers no Stop on a tab's context menu", async () => {
    // The invariant the whole epic turns on: close, end and stop are three
    // different things, and the strip only ever offers the first two. A Stop
    // here would put "remove the tab" and "end the process" one row apart in
    // the same menu, which is the confusion child 2 and 3 exist to remove.
    renderStrip();
    fireEvent.contextMenu(await screen.findByText("alpha"));
    // The menu really did open — otherwise the absence below proves nothing.
    expect(await screen.findByText("Rename Session…")).toBeTruthy();
    expect(screen.queryByText(/stop/i)).toBeNull();
  });
});
