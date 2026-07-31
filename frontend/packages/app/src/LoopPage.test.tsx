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
  // `null` means the detail endpoint 404s — a wrong id, a deleted ticket, or
  // one in another tenant (which answers 404, not 403, so there is no existence
  // oracle). MAIN-296.
  // Populated in `beforeEach` — `vi.hoisted` runs before the consts above.
  task: null as Record<string, unknown> | null,
  /** The status the detail endpoint answers when `task` is null. */
  taskStatus: 404,
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
        return state.task
          ? { data: { task: state.task }, response: { status: 200 } }
          : // openapi-fetch returns no `data` and the real Response on an
            // error status; the page reads the status to tell "not yours to
            // see" from "the request failed".
            { data: undefined, response: { status: state.taskStatus } };
      return { data: null };
    }),
    POST: post,
    PUT: put,
  },
}));

// PARTIAL, on purpose (MAIN-299). The real `ChatView` has to be the thing under
// test — this page's whole job now is to render through it, and a stubbed one
// would let the page "pass" while showing something else entirely. Only the two
// heavy or opaque pieces are replaced: `Panel` (fonts/CSS) and `Markdown`, whose
// stub is what makes "this entry rendered as a document" assertable at all.
vi.mock("@nookos/ui", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@nookos/ui")>()),
  Panel: ({ title, children }: { title?: string; children: React.ReactNode }) => (
    <div>
      <div>{title}</div>
      <div className="nook-panel-body">{children}</div>
    </div>
  ),
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
  state.task = {
    id: TASK_ID,
    key: TASK_KEY,
    title: "Seed and steer",
    type: "task",
  };
  state.taskStatus = 404;
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
    // The page reads the ticket's type, so say what the ticket IS rather than
    // replacing the whole client. This used to call `mockImplementation` and
    // never restore it, so every test that ran afterwards silently saw "An
    // epic" instead of its own fixture — including the not-found tests below,
    // which is how the leak was found (MAIN-296).
    state.task = { id: TASK_ID, key: TASK_KEY, title: "An epic", type: "epic" };
    renderPage();
    expect(
      await screen.findByRole("button", { name: /run decomposer/i }),
    ).toBeTruthy();
  });

  it("posts a steering message to a live run", async () => {
    withJob({ state: "running" }, [line()]);
    renderPage();
    // ChatView's composer, not a bespoke one — that IS the change (AC-2).
    const box = await screen.findByLabelText("Message");

    await userEvent.type(box, "actually, skip the CLI");
    await userEvent.click(screen.getByRole("button", { name: "Send" }));

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

    // AC-3: the question is a MESSAGE in the stream, not a strip beside it.
    const prompt = await screen.findByText("Postgres or Redis?");
    expect(prompt.closest(".chat-msg")).toBeTruthy();
    expect(screen.queryByTestId("asks")).toBeNull();

    // Its choices sit with the composer, and picking one answers it.
    await userEvent.click(screen.getByRole("button", { name: "Postgres" }));
    await waitFor(() => expect(post).toHaveBeenCalled());
    expect(post).toHaveBeenCalledWith("/api/v1/interactions/{id}/answer", {
      params: { path: { id: "ixn-1" } },
      body: { response: "Postgres" },
    });
  });

  it("answers the agent by typing into the same composer (AC-3)", async () => {
    withJob({ state: "waiting_on_human" }, []);
    state.pending = [
      {
        id: "ixn-1",
        task_id: TASK_ID,
        job_id: "job-1",
        prompt: "Which database?",
        choices: [],
        state: "pending",
      },
    ];
    renderPage();

    // One box. With an ask outstanding what you type is the ANSWER to it, and
    // it must not go out as an unprompted steer.
    const box = await screen.findByLabelText("Message");
    await userEvent.type(box, "Postgres, and index the tenant column");
    await userEvent.click(screen.getByRole("button", { name: "Send" }));

    await waitFor(() => expect(post).toHaveBeenCalled());
    expect(post).toHaveBeenCalledWith("/api/v1/interactions/{id}/answer", {
      params: { path: { id: "ixn-1" } },
      body: { response: "Postgres, and index the tenant column" },
    });
    expect(post).not.toHaveBeenCalledWith(
      "/api/v1/jobs/{id}/messages",
      expect.anything(),
    );
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

    // The draft renders as a DOCUMENT — real markdown through ChatView, so
    // `## Acceptance Criteria` is a heading rather than two literal hashes.
    const heading = await screen.findByRole("heading", {
      name: "Acceptance Criteria",
    });
    expect(heading.tagName).toBe("H2");
    expect(heading.closest(".chat-body")?.className).toContain("md");

    // Narration is NOT a document: it stays plain text in a plain body.
    const narration = screen.getByText(/reading the codebase/);
    expect(narration.closest(".chat-body")?.className).not.toContain("md");
    // Exactly ONE body is a document — the draft. (It has two headings of its
    // own, `## Problem` and `## Acceptance Criteria`, so counting headings would
    // measure the fixture rather than the rule.)
    expect(document.querySelectorAll(".chat-body.md")).toHaveLength(1);

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

// MAIN-296: the page must never be a dead box.
//
// The live defect: the `["task", id]` query returned `undefined` for a wrong or
// cross-tenant id. React Query rejects that outright, so `data` stayed
// undefined — indistinguishable from "still loading" — and the composer, which
// is gated on a resolved id, vanished while the transcript fell through to "No
// run yet". A PM landing on a bad id got a message with no input under it and
// no explanation.
describe("not-found and empty states (MAIN-296)", () => {
  /** Catch React Query's own complaint, which is only ever a console line. */
  function watchConsole() {
    const errors: string[] = [];
    const spy = vi
      .spyOn(console, "error")
      .mockImplementation((...args: unknown[]) => {
        errors.push(args.map(String).join(" "));
      });
    return { errors, restore: () => spy.mockRestore() };
  }

  it("a valid ticket with no run renders the seed composer (AC-2/AC-3)", async () => {
    const { errors, restore } = watchConsole();
    renderPage();

    // The message and the box together — the message alone was the regression.
    expect(await screen.findByTestId("composer-seed")).toBeTruthy();
    // Async: the jobs query resolves after the composer mounts, and the point
    // of this test is that the MESSAGE AND THE BOX appear together — the
    // regression was the message alone.
    expect(await screen.findByText(/no run yet/i)).toBeTruthy();
    expect(screen.queryByTestId("loop-not-found")).toBeNull();
    expect((screen.getByTestId("loop-foot") as HTMLElement).hidden).toBe(false);

    restore();
    expect(errors.join("\n")).not.toContain("Query data cannot be undefined");
  });

  it("a ticket that is not yours shows a clean state, not a dead box (AC-1/AC-4)", async () => {
    state.task = null; // the detail endpoint 404s
    const { errors, restore } = watchConsole();
    renderPage();

    const empty = await screen.findByTestId("loop-not-found");
    expect(empty.textContent).toMatch(/doesn't exist, or isn't in your workspace/i);
    // A way out, not just a dead end.
    expect(screen.getByRole("link", { name: /back to the board/i })).toBeTruthy();

    // No composer for a ticket that does not exist — and no empty bar where one
    // would have been.
    expect(screen.queryByTestId("composer-seed")).toBeNull();
    expect(screen.queryByTestId("composer-steer")).toBeNull();
    expect(screen.queryByText(/no run yet/i)).toBeNull();
    // The footer itself is hidden, not merely empty: an empty `lw-foot` still
    // paints as a bar, which is the dead box this card is named for. Asserting
    // "no composer" alone would pass with the bar still on screen.
    expect((screen.getByTestId("loop-foot") as HTMLElement).hidden).toBe(true);

    restore();
    // AC-1, stated as the thing a reader would actually see in the console.
    expect(errors.join("\n")).not.toContain("Query data cannot be undefined");
  });

  it("distinguishes a failed load from a missing ticket (AC-4)", async () => {
    // A 500 is not "no such ticket", and saying so would send someone hunting
    // for a ticket that is perfectly fine.
    state.task = null;
    state.taskStatus = 500;
    const { errors, restore } = watchConsole();
    renderPage();

    const empty = await screen.findByTestId("loop-not-found");
    expect(empty.textContent).toMatch(/could not load/i);
    expect(empty.textContent).not.toMatch(/doesn't exist/i);

    restore();
    expect(errors.join("\n")).not.toContain("Query data cannot be undefined");
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
