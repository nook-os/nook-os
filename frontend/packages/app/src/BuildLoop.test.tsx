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
  decl: { max_replicas: null as number | null },
  runs: [] as { state: string }[],
  putError: null as unknown,
}));

const put = vi.hoisted(() => vi.fn());

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string) => {
      if (path.includes("build-loop")) return { data: state.decl };
      if (path.includes("builds")) return { data: state.runs };
      return { data: null };
    }),
    PUT: put,
  },
}));

import { BuildLoop, buildLoopSummary } from "./BuildLoop";

beforeEach(() => {
  cleanup();
  state.decl = { max_replicas: null };
  state.runs = [];
  state.putError = null;
  put.mockReset();
  put.mockImplementation(async () => ({ error: state.putError ?? undefined }));
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

    await waitFor(() => expect(put).toHaveBeenCalled());
    expect(put.mock.calls[0][1].body).toEqual({ max_replicas: 3 });
  });

  it("clears back to unset — which is not the same write as typing 1", async () => {
    state.decl = { max_replicas: 2 };
    renderPanel();
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "clear" }));

    await waitFor(() => expect(put).toHaveBeenCalled());
    expect(put.mock.calls[0][1].body).toEqual({ max_replicas: null });
  });

  it("offers no clear when there is nothing to clear", async () => {
    renderPanel();
    await screen.findByTestId("build-loop-state");
    expect(screen.queryByRole("button", { name: "clear" })).toBeNull();
  });

  it("surfaces the server's own refusal instead of a guess", async () => {
    state.putError = { error: "max_replicas must be between 0 and 10" };
    renderPanel();
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "set a maximum" }));
    await user.type(screen.getByLabelText("build loop maximum"), "99");
    await user.click(screen.getByRole("button", { name: "save" }));

    const refusal = await screen.findByTestId("build-loop-refusal");
    expect(refusal.textContent).toBe("max_replicas must be between 0 and 10");
  });
});
