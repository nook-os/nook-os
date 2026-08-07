// The per-repo review-loop control (MAIN-447).
//
// The assertions worth having are the ones about states that LOOK alike. An
// unset ceiling and an explicit 1 converge on the same behaviour today, so a
// panel that renders them identically is not obviously wrong — it is only wrong
// later, when somebody goes looking for the switch they never set. Likewise a 0
// rendered as an empty field reads as "nothing here" rather than "this repo is
// deliberately not reviewed".
import React from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

const state = vi.hoisted(() => ({
  decl: { max_replicas: null as number | null },
  status: {
    reconcile_enabled: true,
    loops_enabled: true,
    desired: 1,
    running: 1,
    shortfall: 0,
    port_capped: false,
    blocked: [] as { node_id: string; node_name: string; reason: string }[],
    eligible: 1,
  },
  putError: null as unknown,
}));

const put = vi.hoisted(() => vi.fn());

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string) => {
      if (path.includes("review-loop-status")) return { data: state.status };
      if (path.includes("review-loop")) return { data: state.decl };
      return { data: null };
    }),
    PUT: put,
  },
}));

import { ReviewLoop, reviewLoopGate, reviewLoopSummary } from "./SessionPolicy";

beforeEach(() => {
  cleanup();
  state.decl = { max_replicas: null };
  state.status = {
    reconcile_enabled: true,
    loops_enabled: true,
    desired: 1,
    running: 1,
    shortfall: 0,
    port_capped: false,
    blocked: [],
    eligible: 1,
  };
  state.putError = null;
  put.mockReset();
  put.mockImplementation(async () =>
    state.putError ? { error: state.putError } : { data: state.decl },
  );
});

function renderControl() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <ReviewLoop workspaceId="ws-1" />
    </QueryClientProvider>,
  );
}

describe("reviewLoopSummary — the three states stay three", () => {
  it("renders unset as the default rather than as a bare 1", () => {
    expect(reviewLoopSummary(null).state).toBe("unset (default 1)");
  });

  it("does not render an explicit 1 the same way as unset", () => {
    // The regression this file exists for. Both run one reviewer; only one of
    // them is a decision somebody made.
    expect(reviewLoopSummary(1).state).not.toBe(reviewLoopSummary(null).state);
  });

  it("renders 0 as off, and says what off means for the repo", () => {
    expect(reviewLoopSummary(0).state).toBe("0 (off)");
    expect(reviewLoopSummary(0).detail).toMatch(/not reviewed/);
  });

  it("says a maximum above one runs regardless of open PRs (AC-3)", () => {
    // The terminal already says this; the surface must not be quieter.
    expect(reviewLoopSummary(3).detail).toMatch(/no forge yet, so 3 run/);
  });
});

describe("reviewLoopGate — which switch is off", () => {
  const on = { reconcile_enabled: true, loops_enabled: true } as never;

  it("names nothing when both gates are on", () => {
    expect(reviewLoopGate(on)).toBeNull();
  });

  it("names loops when only loops is off", () => {
    const g = reviewLoopGate({ reconcile_enabled: true, loops_enabled: false } as never);
    expect(g?.key).toBe("loops.enabled");
  });

  it("names reconciling FIRST when both are off", () => {
    // The order `pass()` checks them in. Naming loops first would send somebody
    // to throw a switch that changes nothing while the real one stays off.
    const g = reviewLoopGate({ reconcile_enabled: false, loops_enabled: false } as never);
    expect(g?.key).toBe("sessions.reconcile.enabled");
  });
});

describe("ReviewLoop", () => {
  it("shows the unset state on a fresh workspace", async () => {
    renderControl();
    expect((await screen.findByTestId("review-loop-state")).textContent).toBe(
      "unset (default 1)",
    );
  });

  it("shows 0 as off rather than as an empty field", async () => {
    state.decl = { max_replicas: 0 };
    renderControl();
    expect((await screen.findByTestId("review-loop-state")).textContent).toBe("0 (off)");
  });

  it("shows the shortfall the reconciler reports (AC-4)", async () => {
    state.decl = { max_replicas: 3 };
    state.status = { ...state.status, desired: 3, running: 1, shortfall: 2, eligible: 1 };
    renderControl();
    expect((await screen.findByTestId("review-loop-shortfall")).textContent).toContain(
      "2 short",
    );
  });

  it("names the node a shortfall is waiting on", async () => {
    state.status = {
      ...state.status,
      desired: 2,
      running: 1,
      shortfall: 1,
      blocked: [{ node_id: "n1", node_name: "loop-box", reason: "needs_clone" }],
    };
    renderControl();
    expect((await screen.findByTestId("review-loop-shortfall")).textContent).toContain(
      "loop-box",
    );
  });

  it("explains a port-capped shortfall, which no clone will fix", async () => {
    state.status = { ...state.status, desired: 2, running: 1, shortfall: 1, port_capped: true };
    renderControl();
    expect((await screen.findByTestId("review-loop-shortfall")).textContent).toMatch(
      /declares no ports/,
    );
  });

  it("surfaces the server's own refusal, which names the field", async () => {
    state.putError = { error: "max_replicas must be a non-negative integer" };
    renderControl();
    await userEvent.click(await screen.findByRole("button", { name: /set a maximum/i }));
    await userEvent.type(screen.getByLabelText(/review loop maximum/i), "2");
    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));
    expect((await screen.findByTestId("review-loop-refusal")).textContent).toMatch(
      /max_replicas/,
    );
  });

  it("clears back to unset, which typing 1 cannot do", async () => {
    state.decl = { max_replicas: 2 };
    renderControl();
    await userEvent.click(await screen.findByRole("button", { name: /^clear$/i }));
    await waitFor(() => expect(put).toHaveBeenCalled());
    expect(put.mock.calls[0][1].body).toEqual({ max_replicas: null });
  });

  it("offers no clear button when there is nothing set to clear", async () => {
    renderControl();
    await screen.findByTestId("review-loop-state");
    expect(screen.queryByRole("button", { name: /^clear$/i })).toBeNull();
  });

  it("disables the control and says which switch is off (AC-5)", async () => {
    state.status = { ...state.status, loops_enabled: false };
    renderControl();
    expect((await screen.findByTestId("review-loop-gate")).textContent).toMatch(
      /Loops are off/,
    );
    // Disabled rather than absent — a control that vanishes teaches nothing.
    const btn = screen.getByRole("button", { name: /set a maximum/i }) as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
  });
});
