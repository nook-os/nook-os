// MAIN-128 web surface: the ticket-anchored Loop panel wired to a mocked
// control-plane client. Proves the transcript renders per job state (running /
// waiting / failed / done), that agent narration folds away, that a
// waiting_on_human job raises the reply surface and answering posts to the
// interactions endpoint (which resumes the job server-side), and that the entry
// action is disabled — with the right reason — while a job is already active.
// jsdom only, no control plane.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

const TASK = "task-1";

// Mutable fixture the mocked client reads, reset per test.
const state = vi.hoisted(() => ({
  jobs: [] as unknown[],
  detail: null as unknown,
  pending: [] as unknown[],
}));

const post = vi.hoisted(() => vi.fn(async () => ({ data: {} })));

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string) => {
      if (path === "/api/v1/tasks/{task_id}/jobs") return { data: state.jobs };
      if (path === "/api/v1/jobs/{id}") return { data: state.detail };
      if (path === "/api/v1/interactions") return { data: state.pending };
      return { data: null };
    }),
    POST: post,
  },
}));

import { LoopPanel, LoopActionButton, agentActivityLabel } from "./LoopPanel";
import { loopAction } from "./loop";
import { useLive } from "./live";

function job(over: Record<string, unknown> = {}) {
  return {
    id: "job-1",
    kind: "spec",
    state: "running",
    target_task_id: TASK,
    tenant_id: "t",
    requested_by: "u",
    created_at: "2026-07-27T10:00:00Z",
    updated_at: "2026-07-27T10:00:00Z",
    queued_reason: null,
    predecessor_job_id: null,
    ...over,
  };
}

function line(over: Record<string, unknown> = {}) {
  return {
    id: `line-${Math.random()}`,
    job_id: "job-1",
    source: "system",
    content: "a line",
    at: "2026-07-27T10:00:01Z",
    ...over,
  };
}

function renderPanel(taskType = "task") {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  // The panel now carries an "Open in Loop" link (MAIN-233), so it needs a
  // router in scope like every other linking component.
  return render(
    <MemoryRouter>
      <QueryClientProvider client={qc}>
        <LoopPanel taskId={TASK} taskType={taskType} />
      </QueryClientProvider>
    </MemoryRouter>,
  );
}

function renderAction(taskType = "task") {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <LoopActionButton taskId={TASK} taskType={taskType} />
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  state.jobs = [];
  state.detail = null;
  state.pending = [];
  post.mockClear();
  // The turn map is module state shared across tests; a leftover mark from one
  // test would silence the indicator in the next and read as a passing
  // assertion about the wrong thing.
  useLive.setState({ jobTurn: {} });
});
afterEach(() => cleanup());

describe("loopAction", () => {
  it("offers the decomposer on an epic and a spec elsewhere", () => {
    expect(loopAction("epic", []).kind).toBe("decompose");
    expect(loopAction("epic", []).label).toBe("Run decomposer");
    expect(loopAction("task", []).kind).toBe("spec");
    expect(loopAction("task", []).label).toBe("Draft a spec");
  });

  it("disables with the generic reason when a job is already active", () => {
    const r = loopAction("task", [job({ state: "running" })]);
    expect(r.disabled).toBe(true);
    expect(r.reason).toBe("a loop job is already running on this ticket");
  });

  it("surfaces a queued job's placement reason as the disabled reason", () => {
    const r = loopAction("task", [
      job({ state: "queued", queued_reason: "no eligible executor for this workspace" }),
    ]);
    expect(r.disabled).toBe(true);
    expect(r.reason).toBe("no eligible executor for this workspace");
  });

  it("is enabled once the latest job is terminal", () => {
    expect(loopAction("task", [job({ state: "completed" })]).disabled).toBe(false);
  });
});

describe("LoopPanel transcript states", () => {
  it("shows a running job and folds agent narration away", async () => {
    state.jobs = [job({ state: "running" })];
    state.detail = job({
      state: "running",
      transcript: [
        line({ source: "system", content: "Picked up the ticket" }),
        line({ source: "agent", content: "internal chain of thought" }),
      ],
    });
    renderPanel();

    expect(await screen.findByText("running")).toBeTruthy();
    expect(screen.getByText("Picked up the ticket")).toBeTruthy();
    // Agent narration is hidden until expanded.
    expect(screen.queryByText("internal chain of thought")).toBeNull();
    await userEvent.click(screen.getByText(/Show agent narration/));
    expect(screen.getByText("internal chain of thought")).toBeTruthy();
  });

  it("raises the reply surface on a waiting_on_human job and answering resumes it", async () => {
    state.jobs = [job({ state: "waiting_on_human" })];
    state.detail = job({
      state: "waiting_on_human",
      transcript: [line({ source: "system", content: "Need a decision" })],
    });
    state.pending = [
      {
        id: "ixn-1",
        tenant_id: "t",
        task_id: TASK,
        prompt: "Which branch should I base this on?",
        choices: ["main", "develop"],
        state: "pending",
        created_at: "2026-07-27T10:00:00Z",
        updated_at: "2026-07-27T10:00:00Z",
      },
    ];
    renderPanel();

    expect(await screen.findByText("waiting on human")).toBeTruthy();
    // MAIN-237: the ask now sits above the SHARED composer rather than in its
    // own bespoke block — the prompt and its choices, then one themed input.
    expect(await screen.findByText("Which branch should I base this on?")).toBeTruthy();
    expect(screen.getByLabelText("Message")).toBeTruthy();

    await userEvent.click(screen.getByText("develop"));
    await waitFor(() => expect(post).toHaveBeenCalled());
    expect(post).toHaveBeenCalledWith("/api/v1/interactions/{id}/answer", {
      params: { path: { id: "ixn-1" } },
      body: { response: "develop" },
    });
  });

  it("offers Re-run on a failed job", async () => {
    state.jobs = [job({ state: "failed" })];
    state.detail = job({
      state: "failed",
      transcript: [line({ source: "system", content: "Ran out of budget" })],
    });
    renderPanel();

    expect(await screen.findByText("failed")).toBeTruthy();
    expect(screen.getByText("Ran out of budget")).toBeTruthy();
    await userEvent.click(screen.getByText("Re-run"));
    await waitFor(() => expect(post).toHaveBeenCalled());
    expect(post).toHaveBeenCalledWith("/api/v1/jobs/{id}/rerun", {
      params: { path: { id: "job-1" } },
    });
  });

  it("keeps a completed job's transcript readable with no Re-run", async () => {
    state.jobs = [job({ state: "completed" })];
    state.detail = job({
      state: "completed",
      transcript: [line({ source: "system", content: "Filed 3 sub-tickets" })],
    });
    renderPanel("epic");

    expect(await screen.findByText("done")).toBeTruthy();
    expect(screen.getByText("Filed 3 sub-tickets")).toBeTruthy();
    expect(screen.queryByText("Re-run")).toBeNull();
  });
});

describe("LoopActionButton", () => {
  it("is enabled with no jobs and creates one on click", async () => {
    state.jobs = [];
    renderAction("task");
    const btn = await screen.findByLabelText("Draft a spec");
    await waitFor(() => expect((btn as HTMLButtonElement).disabled).toBe(false));
    await userEvent.click(btn);
    await waitFor(() => expect(post).toHaveBeenCalled());
    expect(post).toHaveBeenCalledWith("/api/v1/jobs", {
      body: { kind: "spec", target_task_id: TASK },
    });
  });

  it("is disabled with the generic reason while a job is active", async () => {
    state.jobs = [job({ state: "running" })];
    renderAction("task");
    const btn = await screen.findByLabelText("Draft a spec");
    await waitFor(() => expect((btn as HTMLButtonElement).disabled).toBe(true));
    expect(btn.getAttribute("title")).toBe("a loop job is already running on this ticket");
  });

  it("surfaces a queued job's reason as the disabled tooltip", async () => {
    state.jobs = [job({ state: "queued", queued_reason: "no eligible executor available" })];
    renderAction("task");
    const btn = await screen.findByLabelText("Draft a spec");
    await waitFor(() => expect((btn as HTMLButtonElement).disabled).toBe(true));
    expect(btn.getAttribute("title")).toBe("no eligible executor available");
  });
});

// MAIN-237: the loop transcript is the shared chat component now, not a fork.
describe("shared chat surface (MAIN-237)", () => {
  it("renders the transcript through ChatView, not the old loop-line rows", async () => {
    state.jobs = [job({ state: "running" })];
    state.detail = job({
      state: "running",
      transcript: [line({ source: "system", content: "dispatched to executor node" })],
    });
    const { container } = renderPanel();

    expect(await screen.findByText("dispatched to executor node")).toBeTruthy();
    // The shared surface…
    expect(container.querySelector(".chat-view")).toBeTruthy();
    expect(container.querySelector(".chat-log")).toBeTruthy();
    // …and none of the fork it replaced.
    expect(container.querySelector(".loop-line")).toBeNull();
  });

  it("shows an activity indicator while the agent works, and not once it stops", async () => {
    state.jobs = [job({ state: "running" })];
    state.detail = job({ state: "running", transcript: [] });
    const { unmount } = renderPanel();
    expect(await screen.findByText("the operator agent is working…")).toBeTruthy();
    unmount();

    // Paused on a human is NOT working — the interaction says what is happening.
    state.jobs = [job({ state: "waiting_on_human" })];
    state.detail = job({ state: "waiting_on_human", transcript: [] });
    const paused = renderPanel();
    await waitFor(() => expect(screen.getByText("waiting on human")).toBeTruthy());
    expect(screen.queryByText("the operator agent is working…")).toBeNull();
    paused.unmount();

    // Neither is a finished one.
    state.jobs = [job({ state: "completed" })];
    state.detail = job({ state: "completed", transcript: [] });
    renderPanel();
    await waitFor(() => expect(screen.getByText("done")).toBeTruthy());
    expect(screen.queryByText("the operator agent is working…")).toBeNull();
  });

  it("believes the real turn signal over the state inference", async () => {
    // A `running` job whose adapter says the agent is BETWEEN turns. State
    // inference has always called this "working" and been wrong about it; that
    // wrongness is the whole reason MAIN-240 exists, so the panel must go quiet.
    state.jobs = [job({ state: "running" })];
    state.detail = job({ state: "running", transcript: [] });
    useLive.setState({ jobTurn: { "job-1": { active: false, at: Date.now() } } });
    const idle = renderPanel();
    // Wait for the panel body, so "no indicator" is a real absence rather than
    // an assertion made before the job detail ever loaded — the way this test
    // would pass while proving nothing.
    await waitFor(() => expect(screen.getByText("running")).toBeTruthy());
    expect(screen.queryByText("the operator agent is working…")).toBeNull();
    idle.unmount();

    // …and the same job with a turn in flight does show it.
    useLive.setState({ jobTurn: { "job-1": { active: true, at: Date.now() } } });
    renderPanel();
    expect(await screen.findByText("the operator agent is working…")).toBeTruthy();
  });

  it("keeps the inferred indicator for a job no adapter reports on (tmux, NG-1)", async () => {
    // The fallback path never sends `job_turn`. Removing the inference along
    // with the guesswork would leave those jobs with no liveness cue at all,
    // which is a regression dressed up as a fix.
    state.jobs = [job({ state: "running" })];
    state.detail = job({ state: "running", transcript: [] });
    useLive.setState({ jobTurn: {} });
    renderPanel();
    expect(await screen.findByText("the operator agent is working…")).toBeTruthy();
  });

  it("answers the pending ask through the shared composer", async () => {
    state.jobs = [job({ state: "waiting_on_human" })];
    state.detail = job({ state: "waiting_on_human", transcript: [] });
    state.pending = [
      {
        id: "ixn-9",
        tenant_id: "t",
        task_id: TASK,
        prompt: "Ship it?",
        choices: [],
        state: "pending",
        created_at: "2026-07-27T10:00:00Z",
        updated_at: "2026-07-27T10:00:00Z",
      },
    ];
    renderPanel();

    const box = await screen.findByLabelText("Message");
    await userEvent.type(box, "yes, ship it");
    await userEvent.click(screen.getByText("Send"));

    await waitFor(() => expect(post).toHaveBeenCalled());
    expect(post).toHaveBeenCalledWith("/api/v1/interactions/{id}/answer", {
      params: { path: { id: "ixn-9" } },
      body: { response: "yes, ship it" },
    });
  });

  it("with nothing to answer, the composer says so instead of taking dead text", async () => {
    state.jobs = [job({ state: "running" })];
    state.detail = job({ state: "running", transcript: [] });
    renderPanel();
    const box = (await screen.findByLabelText("Message")) as HTMLTextAreaElement;
    expect(box.disabled).toBe(true);
    expect(box.placeholder).toBe("Nothing to answer right now");
  });
});

// The label's own truth table (MAIN-240 AC-2). Rendering covers the two cases
// an operator actually sees; this covers the edges cheaply, including the ones
// a render cannot reach — a turn mark left over on a job that has since failed
// is a real state (the node died mid-turn, the reaper failed the job) and the
// indicator must not survive it.
describe("agentActivityLabel", () => {
  const working = "the operator agent is working…";

  it("gates only `running` on the turn signal", () => {
    expect(agentActivityLabel("running")).toBe(working);
    expect(agentActivityLabel("running", { active: true })).toBe(working);
    expect(agentActivityLabel("running", { active: false })).toBeNull();
  });

  it("ignores the turn signal for states that describe the executor, not the agent", () => {
    // No process exists yet, so no turn can be in flight; a stray `false` here
    // must not blank a label that is reporting something else entirely.
    expect(agentActivityLabel("queued", { active: false })).toBe("waiting for an executor…");
    expect(agentActivityLabel("claimed", { active: false })).toBe(
      "the operator agent is starting…",
    );
  });

  it("never resurrects an indicator for a job that is not running", () => {
    for (const state of ["waiting_on_human", "completed", "failed", "canceled"]) {
      expect(agentActivityLabel(state, { active: true })).toBeNull();
    }
  });
});
