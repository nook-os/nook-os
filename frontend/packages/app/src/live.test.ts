// MAIN-240 AC-2: the `job_turn` frame reaches the live store.
//
// The turn signal is the one thing on this branch that terminal-scraping could
// never do honestly, and it is worth an end-of-the-wire test rather than a
// simulated one: the previous review found the event published by the node and
// the control plane and then consumed by nobody, which every unit test on
// either side passed straight through. So this drives the REAL `startLive`
// handler — the socket is mocked, the dispatch is not — and asserts the store.
import { beforeAll, describe, expect, it, vi } from "vitest";
import { QueryClient } from "@tanstack/react-query";

// Capture the frame handler `startLive` registers, so the test can play the
// server's part.
let emit: ((event: unknown) => void) | null = null;

vi.mock("@nookos/api", () => ({
  connectUiSocket: (onEvent: (event: unknown) => void) => {
    emit = onEvent;
    return () => {};
  },
  api: { GET: vi.fn(async () => ({ data: null })) },
}));
vi.mock("./notify", () => ({ notifyEvent: vi.fn(), chimeFor: vi.fn() }));
vi.mock("./secretkeys", () => ({ resyncSealedSecrets: vi.fn() }));

import { startLive, useLive } from "./live";

const JOB = "job-1";
const TASK = "task-1";

const turnFrame = (active: boolean) => ({
  type: "job_turn",
  data: { job_id: JOB, task_id: TASK, active },
});

beforeAll(() => {
  startLive(new QueryClient());
});

describe("job_turn (MAIN-240)", () => {
  it("has a handler at all — the gap the last review found", () => {
    expect(emit).not.toBeNull();
    // Absence is the pre-event state and it is load-bearing: it is what tells
    // the panel "no adapter reported, keep inferring".
    expect(useLive.getState().jobTurn[JOB]).toBeUndefined();
  });

  it("records an active turn, then records the end of it as an explicit false", () => {
    emit!(turnFrame(true));
    expect(useLive.getState().jobTurn[JOB]?.active).toBe(true);

    emit!(turnFrame(false));
    // NOT deleted. `false` and "never reported" mean different things to the
    // indicator, and collapsing them would silently restore the inference for
    // exactly the jobs that have a real signal.
    expect(useLive.getState().jobTurn[JOB]).toBeDefined();
    expect(useLive.getState().jobTurn[JOB]?.active).toBe(false);
  });

  it("keeps jobs apart, so one agent's turn cannot light up another's panel", () => {
    emit!(turnFrame(true));
    emit!({ type: "job_turn", data: { job_id: "job-2", task_id: TASK, active: false } });

    expect(useLive.getState().jobTurn[JOB]?.active).toBe(true);
    expect(useLive.getState().jobTurn["job-2"]?.active).toBe(false);
  });
});
