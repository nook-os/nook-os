// The per-repo build ceiling — the write half MAIN-461 left unbuilt.
//
// The distinction under test throughout is unset-vs-zero: the API reports the
// RAW column so that `null` ("nobody decided") and `0` ("off") stay different
// answers, and a control that collapsed them would silently turn a kill switch
// into a default.
import React from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

const state = vi.hoisted(() => ({
  decl: { concurrency: null as number | null },
  runs: [] as { state: string }[],
  status: null as unknown,
  patchError: null as unknown,
}));

const patch = vi.hoisted(() => vi.fn());

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string) => {
      // Status BEFORE the declaration: `build-loop/status` contains
      // `build-loop`, so the looser match would answer both with the ceiling.
      if (path.includes("build-loop/status")) return { data: state.status };
      if (path.includes("build-loop")) return { data: state.decl };
      // The run listings answer the pagination contract's envelope
      // (MAIN-557), not a bare array.
      if (path.includes("builds")) return { data: { rows: state.runs, next_cursor: null } };
      return { data: null };
    }),
    PATCH: patch,
  },
}));

import { BuildLoop, buildCapacityNote, buildLoopSummary, blockerWords } from "./BuildLoop";

/** A status whose ceiling and fleet agree, which each test then breaks in one
 *  respect — the whole point being that the note appears for a reason. */
const healthy = (over: Record<string, unknown> = {}) => ({
  desired: 1,
  running: 0,
  shortfall: 0,
  capacity: 2,
  eligible_nodes: 1,
  blocked: [],
  ...over,
});

beforeEach(() => {
  cleanup();
  state.decl = { concurrency: null };
  state.runs = [];
  state.status = healthy();
  state.patchError = null;
  patch.mockReset();
  patch.mockImplementation(async () => ({ error: state.patchError ?? undefined }));
});

const renderPanel = () => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <BuildLoop workspaceId="ws-1" />
    </QueryClientProvider>,
  );
};

describe("buildLoopSummary", () => {
  it("keeps unset and zero apart", () => {
    expect(buildLoopSummary(null).state).toBe("unset (default 1)");
    expect(buildLoopSummary(0).state).toBe("0 (off)");
    expect(buildLoopSummary(0).detail).toContain("no build run");
  });

  it("reads singular at one and plural above it", () => {
    expect(buildLoopSummary(1).detail).toBe("one build run at a time");
    expect(buildLoopSummary(3).detail).toBe("up to 3 build runs at once");
    expect(buildLoopSummary(3).state).toBe("max 3");
  });
});

describe("BuildLoop", () => {
  it("shows the unset default rather than claiming somebody chose 1", async () => {
    renderPanel();
    expect((await screen.findByTestId("build-loop-state")).textContent).toBe("unset (default 1)");
  });

  // AC-9: `null` and `0` are the two states a reader must not confuse — one is
  // "nobody decided" and builds one at a time, the other is the kill switch —
  // and the declaration is nullable now, so a component that rendered the raw
  // value would show a blank where the effective ceiling belongs.
  it("renders a null ceiling as unset AND says the effective ceiling is one", async () => {
    renderPanel();
    const line = (await screen.findByTestId("build-loop-state")).parentElement;
    expect(line?.textContent).toContain("unset (default 1)");
    expect(line?.textContent).toContain("one build run at a time");
    expect(line?.textContent).not.toContain("0 (off)");
  });

  it("renders an explicit 0 as the kill switch, never as unset", async () => {
    state.decl = { concurrency: 0 };
    renderPanel();
    const state_ = await screen.findByTestId("build-loop-state");
    expect(state_.textContent).toBe("0 (off)");
    expect(state_.parentElement?.textContent).toContain("no build run is raised for this repo");
  });

  it("counts only RUNNING runs, not every row the panel fetched", async () => {
    state.runs = [{ state: "running" }, { state: "completed" }, { state: "completed" }];
    renderPanel();
    await waitFor(() =>
      expect(screen.getByTestId("build-loop-running").textContent).toBe("1 running"),
    );
  });

  it("writes the typed ceiling", async () => {
    renderPanel();
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "set a maximum" }));
    await user.clear(screen.getByLabelText("build loop maximum"));
    await user.type(screen.getByLabelText("build loop maximum"), "3");
    await user.click(screen.getByRole("button", { name: "save" }));

    await waitFor(() => expect(patch).toHaveBeenCalled());
    expect(patch.mock.calls[0][1].body).toEqual({ concurrency: 3 });
  });

  it("clears back to unset — which is not the same write as typing 1", async () => {
    state.decl = { concurrency: 2 };
    renderPanel();
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "clear" }));

    await waitFor(() => expect(patch).toHaveBeenCalled());
    expect(patch.mock.calls[0][1].body).toEqual({ concurrency: null });
  });

  it("offers no clear when there is nothing to clear", async () => {
    renderPanel();
    await screen.findByTestId("build-loop-state");
    expect(screen.queryByRole("button", { name: "clear" })).toBeNull();
  });

  it("surfaces the server's own refusal instead of a guess", async () => {
    state.patchError = { error: "concurrency must be a non-negative integer, or null to unset" };
    renderPanel();
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "set a maximum" }));
    await user.type(screen.getByLabelText("build loop maximum"), "99");
    await user.click(screen.getByRole("button", { name: "save" }));

    const refusal = await screen.findByTestId("build-loop-refusal");
    expect(refusal.textContent).toBe(
      "concurrency must be a non-negative integer, or null to unset",
    );
  });
});

describe("buildCapacityNote", () => {
  it("says nothing while the ceiling is within what the fleet can deliver", () => {
    expect(buildCapacityNote(healthy({ desired: 2, capacity: 2 }))).toBeNull();
    expect(buildCapacityNote(healthy({ desired: 1, capacity: 2 }))).toBeNull();
  });

  it("names BOTH numbers above capacity, since either alone is unactionable", () => {
    expect(buildCapacityNote(healthy({ desired: 3, capacity: 2 }))).toBe(
      "3 requested \u00b7 your nodes can deliver 2",
    );
  });

  it("states an empty fleet as an absence rather than as a capacity of zero", () => {
    expect(buildCapacityNote(healthy({ desired: 1, capacity: 0, eligible_nodes: 0 }))).toBe(
      "no node of yours accepts build work",
    );
  });

  it("holds its tongue when builds are off for the repo", () => {
    // Nobody asked what the fleet could deliver — the repo asked for nothing.
    expect(buildCapacityNote(healthy({ desired: 0, capacity: 0, eligible_nodes: 0 }))).toBeNull();
  });

  it("says nothing before the status has arrived", () => {
    expect(buildCapacityNote(null)).toBeNull();
    expect(buildCapacityNote(undefined)).toBeNull();
  });
});

describe("blockerWords", () => {
  it("names what a person would go and change", () => {
    expect(blockerWords({ kind: "no_role_label", label: "role/build" })).toBe(
      "no role/build label",
    );
    expect(blockerWords({ kind: "runtime_not_authorized", runtime: "claude" })).toBe(
      "claude is not signed in",
    );
    expect(blockerWords({ kind: "offline" })).toBe("offline");
  });

  it("still names a ground it has never heard of rather than dropping the node", () => {
    expect(blockerWords({ kind: "something_later" })).toBe("not eligible");
  });
});

describe("BuildLoop capacity note", () => {
  it("warns beside a ceiling the fleet cannot honour", async () => {
    state.decl = { concurrency: 3 };
    state.status = healthy({ desired: 3, capacity: 2, eligible_nodes: 1 });
    renderPanel();
    const note = await screen.findByTestId("build-loop-capacity");
    expect(note.textContent).toContain("3 requested");
    expect(note.textContent).toContain("your nodes can deliver 2");
  });

  it("clears the warning once the ceiling comes back within capacity", async () => {
    state.decl = { concurrency: 2 };
    state.status = healthy({ desired: 2, capacity: 2, eligible_nodes: 1 });
    renderPanel();
    await screen.findByTestId("build-loop-state");
    expect(screen.queryByTestId("build-loop-capacity")).toBeNull();
  });

  it("says no node accepts build work rather than showing a capacity of 0", async () => {
    state.status = healthy({ desired: 1, capacity: 0, eligible_nodes: 0 });
    renderPanel();
    const note = await screen.findByTestId("build-loop-capacity");
    expect(note.textContent).toContain("no node of yours accepts build work");
    expect(note.textContent).not.toContain("deliver 0");
  });

  it("names the nodes behind the shortfall and the ground each failed on", async () => {
    state.status = healthy({
      desired: 3,
      capacity: 2,
      eligible_nodes: 1,
      blocked: [
        { node_id: "n1", node_name: "azul", reason: { kind: "no_role_label", label: "role/build" } },
        { node_id: "n2", node_name: "verde", reason: { kind: "offline" } },
      ],
    });
    renderPanel();
    const blocked = await screen.findByTestId("build-loop-blocked");
    expect(blocked.textContent).toContain("azul: no role/build label");
    expect(blocked.textContent).toContain("verde: offline");
  });

  it("still saves a ceiling above capacity — the note is advice, not a gate", async () => {
    state.status = healthy({ desired: 1, capacity: 2, eligible_nodes: 1 });
    renderPanel();
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "set a maximum" }));
    await user.type(screen.getByLabelText("build loop maximum"), "9");
    await user.click(screen.getByRole("button", { name: "save" }));

    await waitFor(() => expect(patch).toHaveBeenCalled());
    expect(patch.mock.calls[0][1].body).toEqual({ concurrency: 9 });
    expect(screen.queryByTestId("build-loop-refusal")).toBeNull();
  });
});
