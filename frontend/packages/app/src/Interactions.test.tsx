// MAIN-159 web surface, as MAIN-600 left it: the top-bar indicator lists the
// pending asks, and a row OPENS one rather than answering it in a 380px panel.
//
// The assertions that matter are about the context. A prompt on its own —
// "which branch?" — is not answerable: what the modal must add is the card that
// asked, the run that asked it, how long it has been stuck, and what that run
// was doing. Each is rendered only when the ask carries it, so the standalone
// ask (neither card nor run) is checked too: it still opens and still answers.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

const PENDING = [
  {
    id: "ixn-1",
    tenant_id: "t",
    task_id: "task-1",
    job_id: "job-1",
    prompt: "Which branch should I base this on?",
    choices: ["main", "develop"],
    state: "pending",
    created_at: "2026-07-27T10:00:00Z",
    updated_at: "2026-07-27T10:00:00Z",
  },
  {
    id: "ixn-2",
    tenant_id: "t",
    prompt: "Proceed with the migration?",
    state: "pending",
    created_at: "2026-07-27T10:01:00Z",
    updated_at: "2026-07-27T10:01:00Z",
  },
];

const TRANSCRIPT = [
  {
    id: "l-1",
    job_id: "job-1",
    source: "agent",
    content: "cloning the workspace",
    at: "2026-07-27T09:59:00Z",
  },
  {
    id: "l-2",
    job_id: "job-1",
    source: "agent",
    content: "two branches look plausible",
    at: "2026-07-27T10:00:00Z",
  },
];

const post = vi.hoisted(() => vi.fn(async () => ({ data: {} })));
const jobs = vi.hoisted(() => [] as string[]);

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string, opts?: { params?: { path?: { id?: string } } }) => {
      if (path === "/api/v1/jobs/{id}") {
        jobs.push(opts?.params?.path?.id ?? "");
        return { data: { transcript: TRANSCRIPT } };
      }
      return { data: PENDING };
    }),
    POST: post,
  },
}));

import { PendingInteractions, waitedFor } from "./Interactions";

function renderIndicator() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <PendingInteractions />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

/** Open the panel and then the ask at `index` — the two clicks the AC names. */
async function openAsk(index: number) {
  renderIndicator();
  await userEvent.click(await screen.findByLabelText("2 pending interactions"));
  await userEvent.click(await screen.findByText(PENDING[index].prompt));
}

beforeEach(() => {
  post.mockClear();
  jobs.length = 0;
});
afterEach(() => cleanup());

describe("PendingInteractions", () => {
  it("badges the pending count and lists the prompts", async () => {
    renderIndicator();
    const trigger = await screen.findByLabelText("2 pending interactions");
    expect(trigger.textContent).toContain("2");
    await userEvent.click(trigger);
    expect(await screen.findByText("Which branch should I base this on?")).toBeTruthy();
    // The answer controls are NOT in the panel any more (AC-8): a queue row has
    // no room for the context an answer needs.
    expect(screen.queryByLabelText("reply")).toBeNull();
  });

  it("opens the ask in a modal, with the card, the run and the wait (AC-9)", async () => {
    await openAsk(0);

    const context = await screen.findByTestId("ixn-context");
    expect(context.textContent).toContain("waiting");
    expect(screen.getByText("open the card").getAttribute("href")).toBe("/loop/task-1");
    expect(context.textContent).toContain("job-1".slice(0, 8));
  });

  it("shows the tail of that run's transcript, through the jobs endpoint (AC-10)", async () => {
    await openAsk(0);

    const tail = await screen.findByTestId("ixn-transcript");
    await waitFor(() => expect(tail.textContent).toContain("two branches look plausible"));
    // The endpoint the runs view already reads, asked for THIS job — no second
    // transcript route exists to drift from it (NG-1).
    expect(jobs).toEqual(["job-1"]);
  });

  it("answers with a structured choice via the answer endpoint (AC-11)", async () => {
    await openAsk(0);
    await userEvent.click(await screen.findByText("develop"));
    await waitFor(() => expect(post).toHaveBeenCalled());
    expect(post).toHaveBeenCalledWith("/api/v1/interactions/{id}/answer", {
      params: { path: { id: "ixn-1" } },
      body: { response: "develop" },
    });
  });

  it("answers a card-less, run-less ask with free text (AC-11, AC-12)", async () => {
    await openAsk(1);

    // The absent context is simply not rendered — a standalone ask is a
    // supported shape, not an error.
    expect(screen.queryByText("open the card")).toBeNull();
    expect(screen.queryByTestId("ixn-transcript")).toBeNull();

    await userEvent.type(screen.getByLabelText("reply"), "yes, go");
    await userEvent.click(screen.getByLabelText("send reply"));
    await waitFor(() => expect(post).toHaveBeenCalled());
    expect(post).toHaveBeenCalledWith("/api/v1/interactions/{id}/answer", {
      params: { path: { id: "ixn-2" } },
      body: { response: "yes, go" },
    });
    // Answered, so the modal is done: leaving it open would be a form for a
    // question nobody is asking any more.
    await waitFor(() => expect(screen.queryByLabelText("reply")).toBeNull());
  });
});

describe("waitedFor", () => {
  const now = Date.parse("2026-07-27T10:30:00Z");

  it("is how long the ask has been waiting", () => {
    expect(waitedFor("2026-07-27T10:00:00Z", now)).toBe("30m");
  });

  it("is absent for a timestamp that will not parse, rather than a wrong number", () => {
    expect(waitedFor("not a date", now)).toBeNull();
  });
});
