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

import { LoopPanel, LoopActionButton } from "./LoopPanel";
import { loopAction } from "./loop";

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
  return render(
    <QueryClientProvider client={qc}>
      <LoopPanel taskId={TASK} taskType={taskType} />
    </QueryClientProvider>,
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
    expect(screen.getByText("The agent is waiting on a human.")).toBeTruthy();
    expect(await screen.findByText("Which branch should I base this on?")).toBeTruthy();

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
