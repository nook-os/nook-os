// The Build loop tab and its Mission Control twin (MAIN-387).
//
// Every assertion here goes through `getByRole` or `isInaccessible`, not
// `getByTestId` alone. Mounted is not the same as visible: a control inside a
// collapsed or `aria-hidden` container satisfies a testid query and is
// unreachable by the person the acceptance criteria are about, and the whole
// point of this card is that these controls are FINDABLE.
import React from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  isInaccessible,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

const state = vi.hoisted(() => ({
  settings: {
    enabled: true,
    concurrency: 1,
    node_id: null as string | null,
    node_name: null as string | null,
    enabled_by: null as string | null,
  } as Record<string, unknown> | null,
  builds: [] as Record<string, unknown>[],
  job: null as Record<string, unknown> | null,
  task: null as Record<string, unknown> | null,
  workspace: { locations: [] as Record<string, unknown>[] },
  nodes: [] as Record<string, unknown>[],
  tenantLoops: true,
  escalated: [] as Record<string, unknown>[],
  patchError: null as unknown,
}));

const patch = vi.hoisted(() => vi.fn());

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string) => {
      // Status first: `build-loop/status` contains `build-loop`, so the looser
      // match would answer both with the declaration.
      if (path.includes("build-loop/status")) return { data: null };
      if (path.includes("build-loop")) return { data: state.settings };
      if (path.endsWith("/builds")) return { data: { rows: state.builds, next_cursor: null } };
      if (path.startsWith("/api/v1/jobs/")) return { data: state.job };
      if (path.startsWith("/api/v1/tasks/")) return { data: state.task };
      if (path === "/api/v1/tasks") return { data: state.escalated };
      if (path === "/api/v1/nodes") return { data: state.nodes };
      if (path === "/api/v1/auth/me") return { data: { person_id: "p1", user: { id: "u1" } } };
      if (path === "/api/v1/settings")
        return {
          data: [{ key: "loops.enabled", scope: "tenant", value: state.tenantLoops }],
        };
      if (path.startsWith("/api/v1/workspaces/")) return { data: state.workspace };
      return { data: null };
    }),
    PATCH: patch,
  },
}));

// A controllable stand-in for the live store, read through the same selector
// API the components use.
const liveState = { jobTurn: {} as Record<string, { active: boolean; at: number }> };
vi.mock("./live", () => ({
  useLive: (sel: (s: typeof liveState) => unknown) => sel(liveState),
}));

import { BuildLoopPanel, MissionBuildLoop } from "./BuildLoopPanel";

const RUNNING = {
  id: "job-1",
  state: "running",
  task_key: "MAIN-42",
  created_at: "2026-08-13T11:59:00Z",
};

beforeEach(() => {
  cleanup();
  state.settings = {
    enabled: true,
    concurrency: 1,
    node_id: null,
    node_name: null,
    enabled_by: null,
  };
  state.builds = [];
  state.job = null;
  state.task = null;
  state.workspace = { locations: [] };
  state.nodes = [];
  state.tenantLoops = true;
  state.escalated = [];
  state.patchError = null;
  liveState.jobTurn = {};
  patch.mockReset();
  patch.mockImplementation(async () => ({ error: state.patchError ?? undefined }));
});

function renderPanel(ui: React.ReactNode = <BuildLoopPanel workspaceId="ws-1" />) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <MemoryRouter>
      <QueryClientProvider client={qc}>{ui}</QueryClientProvider>
    </MemoryRouter>,
  );
}

/** Present is not enough. `isInaccessible` walks the ancestors applying the
 *  same rules a screen reader and a sighted person both lose an element to —
 *  `hidden`, `display:none`, `visibility:hidden`, `aria-hidden`. */
const shown = (el: HTMLElement) => !isInaccessible(el);

describe("the controls (AC-1)", () => {
  it("puts the switch, the pin and the concurrency on one visible panel", async () => {
    renderPanel();

    const sw = await screen.findByRole("button", { name: "build loop: on" });
    expect(shown(sw)).toBe(true);
    const pin = await screen.findByRole("combobox", { name: "build loop node" });
    expect(shown(pin)).toBe(true);
    // The ceiling control is `BuildLoop`, reused rather than reimplemented.
    expect(shown(await screen.findByRole("button", { name: "set a maximum" }))).toBe(true);
  });

  it("defaults the pin to Auto and offers only nodes you own", async () => {
    state.nodes = [
      { id: "n1", name: "azul", owner_person_id: "p1" },
      { id: "n2", name: "somebody-elses", owner_person_id: "p9" },
    ];
    renderPanel();
    const pin = (await screen.findByRole("combobox", {
      name: "build loop node",
    })) as HTMLSelectElement;
    expect(pin.value).toBe("");
    await waitFor(() =>
      expect(screen.getByRole("option", { name: "azul" })).toBeTruthy(),
    );
    expect(screen.queryByRole("option", { name: "somebody-elses" })).toBeNull();
    expect(screen.getByRole("option", { name: /^Auto/ })).toBeTruthy();
  });

  it("keeps a pin at a machine that is not yours listable, so it can be cleared", async () => {
    state.settings = {
      enabled: true,
      concurrency: 1,
      node_id: "n2",
      node_name: "somebody-elses",
      enabled_by: null,
    };
    state.nodes = [{ id: "n2", name: "somebody-elses", owner_person_id: "p9" }];
    renderPanel();
    await waitFor(() =>
      expect(screen.getByRole("option", { name: "somebody-elses" })).toBeTruthy(),
    );
  });

  it("writes only the field that was touched — an absent one leaves the rest alone", async () => {
    state.nodes = [{ id: "n1", name: "azul", owner_person_id: "p1" }];
    renderPanel();
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "build loop: on" }));
    await waitFor(() => expect(patch).toHaveBeenCalled());
    expect(patch.mock.calls[0][1].body).toEqual({ enabled: false });

    patch.mockClear();
    await screen.findByRole("option", { name: "azul" });
    await user.selectOptions(screen.getByRole("combobox", { name: "build loop node" }), "n1");
    await waitFor(() => expect(patch).toHaveBeenCalled());
    expect(patch.mock.calls[0][1].body).toEqual({ node: "n1" });
  });

  it("clears a pin with an explicit null rather than omitting the field", async () => {
    state.settings = {
      enabled: true,
      concurrency: 1,
      node_id: "n1",
      node_name: "azul",
      enabled_by: null,
    };
    state.nodes = [{ id: "n1", name: "azul", owner_person_id: "p1" }];
    renderPanel();
    const user = userEvent.setup();
    await user.selectOptions(
      await screen.findByRole("combobox", { name: "build loop node" }),
      "",
    );
    await waitFor(() => expect(patch).toHaveBeenCalled());
    expect(patch.mock.calls[0][1].body).toEqual({ node: null });
  });
});

describe("why nothing is happening (AC-2)", () => {
  const why = async () => (await screen.findByTestId("build-loop-why")).textContent ?? "";

  it("says an empty board is an absence of work", async () => {
    renderPanel();
    expect(await why()).toContain("no work available");
  });

  it("says the switch is off when it is", async () => {
    state.settings = { enabled: false, concurrency: 1, node_id: null, node_name: null };
    renderPanel();
    expect(await why()).toContain("only when somebody asks");
  });

  it("reports the ceiling once the live runs fill it", async () => {
    state.builds = [RUNNING];
    state.job = { id: "job-1", kind: "build", state: "running", updated_at: "x" };
    renderPanel();
    expect(await why()).toContain("at concurrency");
  });

  it("passes a queued run's own reason through", async () => {
    state.builds = [
      {
        ...RUNNING,
        state: "queued",
        queued_reason: "waiting for node azul, which holds this card's worktree",
      },
    ];
    renderPanel();
    expect(await why()).toContain("waiting for node azul");
  });

  it("says a concluded-nothing run's card is held, and names it", async () => {
    const justNow = new Date(Date.now() - 60_000).toISOString();
    state.builds = [{ ...RUNNING, state: "failed" }];
    state.job = { id: "job-1", kind: "build", state: "failed", updated_at: justNow };
    renderPanel();
    await waitFor(async () => expect(await why()).toContain("backing off until"));
    expect(await why()).toContain("MAIN-42");
  });

  it("shows the live run itself, linked, with its card and node", async () => {
    state.builds = [RUNNING];
    state.job = {
      id: "job-1",
      kind: "build",
      state: "running",
      updated_at: "x",
      target_task_id: "t1",
      executor_node_id: "n1",
    };
    state.nodes = [{ id: "n1", name: "azul", owner_person_id: "p1" }];
    renderPanel();
    const strip = await screen.findByTestId("builder-strip");
    expect(shown(strip)).toBe(true);
    await waitFor(() =>
      expect(screen.getByTestId("builder-node").textContent).toContain("azul"),
    );
    expect(screen.getByTestId("build-ticket").getAttribute("href")).toBe("/loop/t1");
  });
});

describe("the tenant switch (AC-5)", () => {
  it("says exactly that, and links to the page that fixes it", async () => {
    state.tenantLoops = false;
    renderPanel();
    const notice = await screen.findByTestId("build-loop-tenant-off");
    expect(shown(notice)).toBe(true);
    expect(notice.textContent).toContain("Loops are off for this tenant");
    const link = screen.getByRole("link", { name: /Settings/ });
    expect(link.getAttribute("href")).toBe("/settings?section=automation");
  });

  it("stays up after the repo's own switch is turned on — the tenant still gates it", async () => {
    state.tenantLoops = false;
    state.settings = { enabled: false, concurrency: 1, node_id: null, node_name: null };
    renderPanel();
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "build loop: off" }));
    await waitFor(() => expect(patch).toHaveBeenCalled());
    expect(shown(screen.getByTestId("build-loop-tenant-off"))).toBe(true);
  });

  it("never flashes the diagnosis before the setting has been read", async () => {
    renderPanel();
    await screen.findByRole("button", { name: "build loop: on" });
    expect(screen.queryByTestId("build-loop-tenant-off")).toBeNull();
  });
});

describe("refusals (AC-6)", () => {
  it("shows the server's own words instead of failing silently", async () => {
    state.patchError = { error: "a node token cannot do this — sign in as a user" };
    renderPanel();
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "build loop: on" }));

    const refusal = await screen.findByTestId("build-loop-switch-refusal");
    expect(shown(refusal)).toBe(true);
    expect(refusal.textContent).toBe("a node token cannot do this — sign in as a user");
  });

  it("refuses the pin in the same words, on the same surface", async () => {
    state.patchError = { error: 'no node named "azul" in this tenant' };
    state.nodes = [{ id: "n1", name: "azul", owner_person_id: "p1" }];
    renderPanel();
    const user = userEvent.setup();
    await screen.findByRole("option", { name: "azul" });
    await user.selectOptions(
      screen.getByRole("combobox", { name: "build loop node" }),
      "n1",
    );
    expect((await screen.findByTestId("build-loop-switch-refusal")).textContent).toContain(
      "no node named",
    );
  });

  it("clears the refusal once a write is accepted", async () => {
    state.patchError = { error: "nope" };
    renderPanel();
    const user = userEvent.setup();
    const flip = await screen.findByRole("button", { name: "build loop: on" });
    await user.click(flip);
    await screen.findByTestId("build-loop-switch-refusal");

    state.patchError = null;
    await user.click(flip);
    await waitFor(() =>
      expect(screen.queryByTestId("build-loop-switch-refusal")).toBeNull(),
    );
  });
});

describe("escalated cards (AC-4)", () => {
  it("flags them, with the card — and its escalation comment — one click away", async () => {
    state.escalated = [{ id: "t1", key: "MAIN-99", title: "Something the loop gave up on" }];
    renderPanel();
    const flag = await screen.findByTestId("build-loop-escalated");
    expect(shown(flag)).toBe(true);
    expect(flag.textContent).toContain("1 card escalated to a human");
    expect(screen.getByRole("link", { name: "MAIN-99" }).getAttribute("href")).toBe(
      "/board?task=MAIN-99",
    );
  });

  it("says nothing when nothing is escalated", async () => {
    renderPanel();
    await screen.findByTestId("build-loop-why");
    expect(screen.queryByTestId("build-loop-escalated")).toBeNull();
  });
});

describe("Mission Control (AC-3, AC-8)", () => {
  it("shows the switch and the pin per repo, and flips without leaving the page", async () => {
    state.settings = {
      enabled: true,
      concurrency: 1,
      node_id: "n1",
      node_name: "azul",
      enabled_by: null,
    };
    renderPanel(<MissionBuildLoop workspaceId="ws-1" />);

    expect(shown(await screen.findByTestId("mission-build-pin"))).toBe(true);
    expect(screen.getByTestId("mission-build-pin").textContent).toBe("azul");

    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "build loop: on" }));
    await waitFor(() => expect(patch).toHaveBeenCalled());
    expect(patch.mock.calls[0][1].body).toEqual({ enabled: false });
  });

  it("reads Auto when nothing is pinned", async () => {
    renderPanel(<MissionBuildLoop workspaceId="ws-1" />);
    expect((await screen.findByTestId("mission-build-pin")).textContent).toBe("Auto");
  });

  it("carries the builder strip beside the switch", async () => {
    state.builds = [RUNNING];
    state.job = {
      id: "job-1",
      kind: "build",
      state: "running",
      target_task_id: "t1",
      executor_node_id: "n1",
      updated_at: "x",
    };
    state.nodes = [{ id: "n1", name: "azul", owner_person_id: "p1" }];
    renderPanel(<MissionBuildLoop workspaceId="ws-1" />);
    expect(shown(await screen.findByTestId("builder-strip"))).toBe(true);
    expect(screen.getByTestId("build-ticket").textContent).toBe("MAIN-42");
  });

  it("surfaces a refusal here too, rather than a chip that silently does nothing", async () => {
    state.patchError = { error: "this needs tenant owner or admin" };
    renderPanel(<MissionBuildLoop workspaceId="ws-1" />);
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "build loop: on" }));
    expect((await screen.findByTestId("mission-build-refusal")).textContent).toBe(
      "this needs tenant owner or admin",
    );
  });
});

describe("the builder strip's turn signal (AC-8)", () => {
  beforeEach(() => {
    state.builds = [RUNNING];
    state.job = { id: "job-1", kind: "build", state: "running", updated_at: "x" };
  });

  it("says the agent is working while a turn is in flight", async () => {
    liveState.jobTurn = { "job-1": { active: true, at: Date.now() } };
    renderPanel();
    expect((await screen.findByTestId("builder-activity")).textContent).toContain("working");
  });

  it("goes quiet between turns rather than claiming work that is not happening", async () => {
    liveState.jobTurn = { "job-1": { active: false, at: Date.now() } };
    renderPanel();
    await screen.findByTestId("builder-strip");
    expect(screen.queryByTestId("builder-activity")).toBeNull();
  });

  it("keeps the inferred indicator when no adapter has reported at all", async () => {
    renderPanel();
    expect((await screen.findByTestId("builder-activity")).textContent).toContain("working");
  });
});

describe("what a build run produced (AC-7)", () => {
  it("shows the ticket, branch and PR while the run is still going", async () => {
    state.builds = [RUNNING];
    state.job = {
      id: "job-1",
      kind: "build",
      state: "running",
      target_task_id: "t1",
      updated_at: "x",
    };
    state.task = {
      task: {
        id: "t1",
        key: "MAIN-42",
        worktree_path: "/srv/repo__main-42",
        pr_url: "https://github.com/nook-os/nook-os/pull/443",
      },
    };
    state.workspace = {
      locations: [{ path: "/srv/repo__main-42", git_branch: "main-42-build-loop-ui" }],
    };
    renderPanel();
    await waitFor(() =>
      expect(screen.getByTestId("build-branch").textContent).toContain(
        "main-42-build-loop-ui",
      ),
    );
    const pr = screen.getByTestId("build-pr");
    expect(shown(pr)).toBe(true);
    expect(pr.textContent).toContain("PR #443");
    expect(pr.getAttribute("href")).toBe("https://github.com/nook-os/nook-os/pull/443");
  });
});
