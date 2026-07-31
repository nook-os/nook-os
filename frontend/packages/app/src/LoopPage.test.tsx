// MAIN-233: the Loop workspace against a mocked control plane. Proves the four
// things the page exists to do — start a run WITH a typed idea (AC-2), post a
// steering message to a live run (AC-3), raise an interaction's answer controls
// inline (AC-3), and render a drafted issue as markdown with a link back to the
// ticket it filed (AC-4) — plus the honest empty/terminal states (AC-5) and
// that a live `job_changed` echo repaints without a reload.
// jsdom only, no control plane.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

// The route param the board menu uses is the KEY; the uuid is what interaction
// rows carry and what `live.ts` invalidates. Keeping them DIFFERENT here is the
// point: identical values masked a defect where the page keyed on the key and
// so never rendered asks or heard the live events.
const TASK_KEY = "MAIN-42";
const TASK_ID = "019fafd1-7667-70a3-9cdd-84f8f5a561b5";

const state = vi.hoisted(() => ({
  jobs: [] as unknown[],
  detail: null as unknown,
  pending: [] as unknown[],
  /** The tenant's `loops.enabled` setting, as `/settings` reports it. */
  loopsOn: false,
}));

const post = vi.hoisted(() => vi.fn(async () => ({ data: {} })));
const put = vi.hoisted(() => vi.fn(async () => ({ data: {} }) as Record<string, unknown>));

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string) => {
      if (path === "/api/v1/tasks/{task_id}/jobs") return { data: state.jobs };
      if (path === "/api/v1/jobs/{id}") return { data: state.detail };
      if (path === "/api/v1/interactions") return { data: state.pending };
      if (path === "/api/v1/settings")
        return {
          data: [{ key: "loops.enabled", scope: "tenant", value: state.loopsOn }],
        };
      if (path === "/api/v1/tasks/{id}")
        return {
          data: {
            task: { id: TASK_ID, key: TASK_KEY, title: "Seed and steer", type: "task" },
          },
        };
      return { data: null };
    }),
    POST: post,
    PUT: put,
  },
}));

// The real UI package pulls fonts and CSS; the page only needs three shapes.
vi.mock("@nookos/ui", () => ({
  Panel: ({ title, children }: { title?: string; children: React.ReactNode }) => (
    <div>
      <div>{title}</div>
      <div className="nook-panel-body">{children}</div>
    </div>
  ),
  Empty: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
  Markdown: ({ src }: { src: string }) => <div data-testid="md">{src}</div>,
}));

import { LoopPage } from "./pages/Loop";

function job(over: Record<string, unknown> = {}) {
  return {
    id: "job-1",
    kind: "spec",
    state: "running",
    target_task_id: TASK_ID,
    tenant_id: "t",
    requested_by: "u",
    seed: null,
    queued_reason: null,
    predecessor_job_id: null,
    executor_node_id: null,
    workspace_id: null,
    created_at: "2026-07-29T10:00:00Z",
    updated_at: "2026-07-29T10:00:00Z",
    ...over,
  };
}

let lineNo = 0;
function line(over: Record<string, unknown> = {}) {
  return {
    id: `line-${++lineNo}`,
    job_id: "job-1",
    source: "agent",
    content: "working…",
    at: "2026-07-29T10:00:01Z",
    ...over,
  };
}

/** Put a job (and optionally its transcript) in front of the page. */
function withJob(over: Record<string, unknown> = {}, transcript: unknown[] = []) {
  const j = job(over);
  state.jobs = [j];
  state.detail = { ...j, transcript };
  return j;
}

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const utils = render(
    <MemoryRouter initialEntries={[`/loop/${TASK_KEY}`]}>
      <QueryClientProvider client={qc}>
        <Routes>
          <Route path="/loop/:taskId" element={<LoopPage />} />
        </Routes>
      </QueryClientProvider>
    </MemoryRouter>,
  );
  return { ...utils, qc };
}

beforeEach(() => {
  state.jobs = [];
  state.detail = null;
  state.pending = [];
  state.loopsOn = false;
  post.mockClear();
  put.mockClear();
  put.mockImplementation(async () => ({ data: {} }));
});
afterEach(cleanup);

describe("Loop workspace (MAIN-233)", () => {
  it("with no run, offers the seed box and starts the job WITH the typed idea", async () => {
    renderPage();
    const box = await screen.findByLabelText(/what do you want out of this run/i);

    await userEvent.type(box, "focus on the migration path");
    await userEvent.click(screen.getByRole("button", { name: /draft a spec/i }));

    await waitFor(() => expect(post).toHaveBeenCalled());
    expect(post).toHaveBeenCalledWith("/api/v1/jobs", {
      body: {
        kind: "spec",
        target_task_id: TASK_ID,
        seed: "focus on the migration path",
      },
    });
  });

  it("omits the seed entirely when the box is left empty", async () => {
    renderPage();
    await screen.findByTestId("composer-seed");
    await userEvent.click(screen.getByRole("button", { name: /draft a spec/i }));

    await waitFor(() => expect(post).toHaveBeenCalled());
    expect(post).toHaveBeenCalledWith("/api/v1/jobs", {
      body: { kind: "spec", target_task_id: TASK_ID },
    });
  });

  it("an epic offers the decomposer, not a spec", async () => {
    // The page reads the ticket's type; override just that response.
    const { api } = await import("@nookos/api");
    (api.GET as ReturnType<typeof vi.fn>).mockImplementation(async (path: string) => {
      if (path === "/api/v1/tasks/{task_id}/jobs") return { data: state.jobs };
      if (path === "/api/v1/jobs/{id}") return { data: state.detail };
      if (path === "/api/v1/interactions") return { data: state.pending };
      if (path === "/api/v1/settings")
        return {
          data: [{ key: "loops.enabled", scope: "tenant", value: state.loopsOn }],
        };
      if (path === "/api/v1/tasks/{id}")
        return {
          data: { task: { id: TASK_ID, key: TASK_KEY, title: "An epic", type: "epic" } },
        };
      return { data: null };
    });
    renderPage();
    expect(
      await screen.findByRole("button", { name: /run decomposer/i }),
    ).toBeTruthy();
  });

  it("posts a steering message to a live run", async () => {
    withJob({ state: "running" }, [line()]);
    renderPage();
    const box = await screen.findByLabelText("message the agent");

    await userEvent.type(box, "actually, skip the CLI");
    await userEvent.click(screen.getByRole("button", { name: "send message" }));

    await waitFor(() => expect(post).toHaveBeenCalled());
    expect(post).toHaveBeenCalledWith("/api/v1/jobs/{id}/messages", {
      params: { path: { id: "job-1" } },
      body: { body: "actually, skip the CLI" },
    });
  });

  it("steers a run that is paused on a human too — that is what resumes it", async () => {
    withJob({ state: "waiting_on_human" }, []);
    renderPage();
    expect(await screen.findByTestId("composer-steer")).toBeTruthy();
  });

  it("renders a pending interaction inline with its answer controls", async () => {
    withJob({ state: "waiting_on_human" }, []);
    state.pending = [
      {
        id: "ixn-1",
        task_id: TASK_ID,
        job_id: "job-1",
        prompt: "Postgres or Redis?",
        choices: ["Postgres", "Redis"],
        state: "pending",
      },
    ];
    renderPage();

    expect(await screen.findByTestId("asks")).toBeTruthy();
    expect(screen.getByText("Postgres or Redis?")).toBeTruthy();

    await userEvent.click(screen.getByRole("button", { name: "Postgres" }));
    await waitFor(() => expect(post).toHaveBeenCalled());
    expect(post).toHaveBeenCalledWith("/api/v1/interactions/{id}/answer", {
      params: { path: { id: "ixn-1" } },
      body: { response: "Postgres" },
    });
  });

  it("renders a drafted issue as markdown and links back to what it filed", async () => {
    withJob({ state: "waiting_on_human" }, [
      line({ source: "agent", content: "reading the codebase…" }),
      line({
        source: "agent",
        content:
          "## Problem\n\nNo composer.\n\n## Acceptance Criteria\n\n- [ ] AC-1 — a box\n",
      }),
      line({
        source: "system",
        content: "Filed MAIN-99 — the composer. NG-1 held; encoded as UTF-8.",
      }),
    ]);
    renderPage();

    // The draft entry renders through Markdown; the narration line does not.
    const md = await screen.findByTestId("md");
    expect(md.textContent).toContain("## Acceptance Criteria");
    expect(screen.getAllByTestId("md")).toHaveLength(1);
    expect(screen.getByText(/reading the codebase/)).toBeTruthy();

    // The filed ticket is a link back; the job's own target is not offered.
    const filed = screen.getByTitle("open MAIN-99");
    expect(filed.getAttribute("href")).toBe("/board?task=MAIN-99");
    expect(screen.queryByTitle(`open ${TASK_KEY}`)).toBeNull();
    // …and the draft's own AC-N / NG-N tags are not mistaken for tickets.
    expect(screen.queryByTitle("open AC-1")).toBeNull();
    expect(screen.queryByTitle("open NG-1")).toBeNull();
  });

  // Both halves of the uuid-vs-key fix, on the board menu's entry path (the
  // route param here is the KEY). The asks test above already proves the
  // filter; this proves the JOBS list is keyed on the uuid, which is what
  // `job_changed` invalidates and what the composer's mode is derived from.
  it("hears a live job_changed on the uuid key, even when routed by board key", async () => {
    withJob({ state: "running" }, []);
    const { qc } = renderPage();
    expect(await screen.findByTestId("composer-steer")).toBeTruthy();

    // The run finishes. `live.ts` invalidates by UUID — never by key.
    withJob({ state: "completed" }, [line({ source: "system", content: "done" })]);
    qc.invalidateQueries({ queryKey: ["task", TASK_ID, "jobs"] });

    // The composer notices, because the list it reads is keyed the same way.
    expect(await screen.findByTestId("composer-readonly")).toBeTruthy();
  });

  it("repaints when the live event invalidates the job — no reload", async () => {
    withJob({ state: "running" }, [line({ source: "human", content: "first" })]);
    const { qc } = renderPage();
    expect(await screen.findByText("first")).toBeTruthy();

    // What `live.ts` does on a `job_changed` frame: mark the job stale.
    state.detail = {
      ...(state.detail as Record<string, unknown>),
      transcript: [
        line({ source: "human", content: "first" }),
        line({ source: "agent", content: "second" }),
      ],
    };
    qc.invalidateQueries({ queryKey: ["job"] });

    expect(await screen.findByText("second")).toBeTruthy();
  });

  it("a completed run is read-only, and a failed one offers a re-run", async () => {
    withJob({ state: "completed" }, [line({ source: "system", content: "done" })]);
    const { unmount } = renderPage();
    expect(await screen.findByTestId("composer-readonly")).toBeTruthy();
    expect(screen.queryByLabelText("message the agent")).toBeNull();
    expect(screen.queryByRole("button", { name: /re-run/i })).toBeNull();
    unmount();

    withJob({ state: "failed" }, [line({ source: "system", content: "boom" })]);
    renderPage();
    await screen.findByTestId("composer-readonly");
    await userEvent.click(screen.getByRole("button", { name: /re-run/i }));
    await waitFor(() => expect(post).toHaveBeenCalled());
    expect(post).toHaveBeenCalledWith("/api/v1/jobs/{id}/rerun", {
      params: { path: { id: "job-1" } },
    });
  });
});

// MAIN-297: a queued run that cannot be placed says WHY, and offers the fix
// here. Both causes are fixed on OTHER pages, which is what made this a dead
// end: a run blocked by `loops.enabled=false` is only unblockable from
// Settings→Loops, and nothing on the run said so.
describe("stuck-run diagnosis (MAIN-297)", () => {
  it("loops off: names the cause and turns it on without leaving the page", async () => {
    state.loopsOn = false;
    withJob({ state: "queued" }, []);
    renderPage();

    const notice = await screen.findByTestId("stuck-loops-off");
    expect(notice.textContent).toMatch(/loops are off/i);

    await userEvent.click(screen.getByRole("button", { name: /turn on loops/i }));

    await waitFor(() => expect(put).toHaveBeenCalled());
    expect(put).toHaveBeenCalledWith("/api/v1/settings/{key}", {
      params: { path: { key: "loops.enabled" } },
      body: { value: true, scope: "tenant" },
    });
    // Still on the run — the fix does not navigate away (AC-2).
    expect(screen.getByTestId("loop-workspace")).toBeTruthy();
  });

  it("loops off wins over a stale executor reason", async () => {
    // The dispatcher does not poll while loops are off, so any reason on the
    // row predates the switch. Pointing at Nodes would send the reader to fix
    // something that is not the problem.
    state.loopsOn = false;
    withJob(
      {
        state: "queued",
        queued_reason: "no eligible executor: you have no node online",
      },
      [],
    );
    renderPage();

    expect(await screen.findByTestId("stuck-loops-off")).toBeTruthy();
    expect(screen.queryByTestId("stuck-no-executor")).toBeNull();
  });

  it("loops on, nothing eligible: shows the backend's reason and a Nodes link", async () => {
    state.loopsOn = true;
    const detail =
      "no eligible executor: your online node(s) are not authorized for the claude runtime";
    withJob({ state: "queued", queued_reason: detail }, []);
    renderPage();

    const notice = await screen.findByTestId("stuck-no-executor");
    // The backend already distinguishes the sub-causes; the page must not
    // re-word it and drift from the source.
    expect(notice.textContent).toContain(detail);
    expect(screen.getByRole("link", { name: /open nodes/i })).toBeTruthy();
    expect(screen.queryByRole("button", { name: /turn on loops/i })).toBeNull();
  });

  it("an undiagnosed queued run still just waits (AC-3)", async () => {
    state.loopsOn = true;
    withJob({ state: "queued", queued_reason: null }, []);
    renderPage();

    expect(await screen.findByTestId("stuck-waiting")).toBeTruthy();
    expect(screen.queryByTestId("stuck-loops-off")).toBeNull();
    expect(screen.queryByTestId("stuck-no-executor")).toBeNull();
  });

  it("a refused turn-on says who can fix it instead of failing silently (AC-4)", async () => {
    // openapi-fetch reports HTTP failures in `error` rather than throwing, so a
    // 403 would otherwise look exactly like success and the notice would just
    // sit there.
    state.loopsOn = false;
    put.mockImplementation(async () => ({ error: { message: "forbidden" } }));
    withJob({ state: "queued" }, []);
    renderPage();

    await screen.findByTestId("stuck-loops-off");
    await userEvent.click(screen.getByRole("button", { name: /turn on loops/i }));

    const failed = await screen.findByTestId("stuck-loops-off-failed");
    expect(failed.textContent).toMatch(/permission|owner/i);
  });

  it("says nothing over a run that is already going", async () => {
    state.loopsOn = false; // off, but this run already has an executor
    withJob({ state: "running" }, [line()]);
    renderPage();

    await screen.findByTestId("composer-steer");
    expect(screen.queryByTestId("stuck-loops-off")).toBeNull();
    expect(screen.queryByTestId("stuck-waiting")).toBeNull();
  });
});
