// The repo's ONE runs surface (MAIN-488), carrying the coverage the separate
// Reviews (MAIN-455 AC-5) and Builds (MAIN-461 AC-2) suites held.
//
// What is worth pinning: a run is READ, not driven; two runs of the same PR are
// distinguishable; a build names its card and falls back for a keyless one; and
// — new here — the two kinds share one list, one empty state and one filter,
// with the kind in the URL so an old link still lands somewhere true.
import React from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter, useLocation } from "react-router-dom";

const state = vi.hoisted(() => ({
  builds: [] as unknown[],
  reviews: [] as unknown[],
  transcript: [] as unknown[],
}));

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string) => {
      if (path.includes("/builds")) return { data: state.builds };
      if (path.includes("/reviews")) return { data: state.reviews };
      if (path.includes("/jobs/")) return { data: { transcript: state.transcript } };
      return { data: null };
    }),
  },
}));

import {
  mergeRuns,
  parseKind,
  pillTone,
  runLabel,
  shortHead,
  useLegacyRunsSectionRedirect,
  WorkspaceRuns,
} from "./WorkspaceRuns";

const review = (over: Record<string, unknown> = {}) => ({
  id: "job-1",
  state: "completed",
  review_pr_number: 341,
  review_head_sha: "abcdef1234567890",
  created_at: "2026-08-08T10:00:00Z",
  ...over,
});

const build = (over: Record<string, unknown> = {}) => ({
  id: "job-b1",
  state: "running",
  task_key: "MAIN-42",
  created_at: "2026-08-08T09:00:00Z",
  ...over,
});

beforeEach(() => {
  cleanup();
  state.builds = [];
  state.reviews = [];
  state.transcript = [];
});

/** Rendered, and actually SEEN — a control hidden by its own style or by an
 *  ancestor's is not offered, however present it is in the DOM. */
function isVisible(el: Element | null): boolean {
  if (!el) return false;
  for (let n: Element | null = el; n && n !== document.body; n = n.parentElement) {
    if (n instanceof HTMLElement) {
      if (n.hidden) return false;
      const style = getComputedStyle(n);
      if (style.display === "none" || style.visibility === "hidden") return false;
    }
  }
  return true;
}

function Search() {
  return <span data-testid="search">{useLocation().search}</span>;
}

function renderRuns(url = "/workspaces/ws-1") {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <MemoryRouter initialEntries={[url]}>
      <QueryClientProvider client={qc}>
        <WorkspaceRuns workspaceId="ws-1" />
        <Search />
      </QueryClientProvider>
    </MemoryRouter>,
  );
  return qc;
}

/** What `live.ts` does to this panel when a `job_changed` frame arrives: mark
 *  both listing keys stale. The panel is mounted, so they refetch. */
async function jobChanged(qc: QueryClient) {
  await act(async () => {
    await qc.invalidateQueries({ queryKey: ["workspace-builds"] });
    await qc.invalidateQueries({ queryKey: ["workspace-reviews"] });
  });
}

const search = () => screen.getByTestId("search").textContent;
const kindOf = (row: HTMLElement) => row.getAttribute("data-kind");

describe("row identity", () => {
  it("names the pull request a review owns", () => {
    expect(runLabel(review() as never)).toBe("PR #341");
  });

  it("does not invent a PR for a run that has none", () => {
    expect(runLabel(review({ review_pr_number: null }) as never)).toBe("review");
  });

  it("shortens the head, which is what tells two runs of one PR apart", () => {
    expect(shortHead("abcdef1234567890")).toBe("abcdef1");
    expect(shortHead(null)).toBe("");
  });

  it("maps the loop's muted onto the design system's dim", () => {
    // The two vocabularies differ by exactly this one word; anything else
    // passing through unchanged is the point.
    expect(pillTone("muted")).toBe("dim");
    expect(pillTone("err")).toBe("err");
  });
});

describe("mergeRuns", () => {
  it("interleaves the two kinds newest first", () => {
    const rows = mergeRuns(
      [build({ id: "b1", created_at: "2026-08-08T12:00:00Z" })] as never,
      [
        review({ id: "r1", created_at: "2026-08-08T13:00:00Z" }),
        review({ id: "r2", created_at: "2026-08-08T11:00:00Z" }),
      ] as never,
    );
    expect(rows.map((r) => r.id)).toEqual(["r1", "b1", "r2"]);
    expect(rows.map((r) => r.kind)).toEqual(["review", "build", "review"]);
  });

  it("orders two runs raised in the same instant stably", () => {
    const at = "2026-08-08T10:00:00Z";
    const rows = mergeRuns(
      [build({ id: "b2", created_at: at }), build({ id: "b1", created_at: at })] as never,
      [] as never,
    );
    expect(rows.map((r) => r.id)).toEqual(["b1", "b2"]);
  });

  it("reads an unknown kind word in the URL as no filter", () => {
    expect(parseKind(null)).toBe("all");
    expect(parseKind("specs")).toBe("all");
    expect(parseKind("review")).toBe("review");
  });
});

describe("one list, both kinds", () => {
  it("shows builds and reviews together, newest first, each naming its kind", async () => {
    state.builds = [build({ created_at: "2026-08-08T09:00:00Z" })];
    state.reviews = [review({ created_at: "2026-08-08T10:00:00Z" })];
    renderRuns();
    const rows = await screen.findAllByTestId("run-row");
    expect(rows).toHaveLength(2);
    expect(kindOf(rows[0])).toBe("review");
    expect(rows[0].textContent).toContain("review");
    expect(rows[0].textContent).toContain("PR #341");
    expect(rows[0].textContent).toContain("abcdef1");
    expect(kindOf(rows[1])).toBe("build");
    expect(rows[1].textContent).toContain("build");
    expect(rows[1].textContent).toContain("MAIN-42");
  });

  it("names a build row by card key, falls back for a keyless run, and shows the outcome", async () => {
    // A keyless build is a deleted card — or a private one whose key the
    // listing withholds from this viewer.
    state.builds = [
      build(),
      build({
        id: "job-b2",
        state: "completed",
        task_key: null,
        created_at: "2026-08-08T08:00:00Z",
        build_outcome: "pr_opened",
      }),
    ];
    renderRuns();
    const rows = await screen.findAllByTestId("run-row");
    expect(rows[0].textContent).toContain("MAIN-42");
    expect(rows[1].textContent).toContain("pr_opened");
  });

  it("shows two runs of ONE pull request as two entries", async () => {
    // The wakeup rule made visible: same PR, different head, so a list of two
    // is two pushes rather than the loop spinning.
    state.reviews = [review(), review({ id: "job-2", review_head_sha: "999888777" })];
    renderRuns();
    const rows = await screen.findAllByTestId("run-row");
    expect(rows).toHaveLength(2);
    expect(rows[0].textContent).not.toEqual(rows[1].textContent);
  });

  it("offers both filters, visibly, and starts with neither applied", async () => {
    state.builds = [build()];
    state.reviews = [review()];
    renderRuns();
    await screen.findAllByTestId("run-row");
    const kind = screen.getByLabelText("filter by kind") as HTMLSelectElement;
    const runState = screen.getByLabelText("filter by state") as HTMLSelectElement;
    expect(isVisible(kind)).toBe(true);
    expect(isVisible(runState)).toBe(true);
    expect(kind.value).toBe("all");
    expect(runState.value).toBe("all");
    expect(search()).toBe("");
  });
});

describe("filtering", () => {
  beforeEach(() => {
    state.builds = [build(), build({ id: "job-b2", state: "completed", task_key: "MAIN-43" })];
    state.reviews = [review()];
  });

  it("narrows to one kind and records the choice in the URL", async () => {
    renderRuns();
    await screen.findAllByTestId("run-row");
    fireEvent.change(screen.getByLabelText("filter by kind"), { target: { value: "build" } });
    let rows = await screen.findAllByTestId("run-row");
    expect(rows).toHaveLength(2);
    expect(rows.every((r) => kindOf(r) === "build")).toBe(true);
    expect(search()).toContain("kind=build");

    fireEvent.change(screen.getByLabelText("filter by kind"), { target: { value: "review" } });
    rows = await screen.findAllByTestId("run-row");
    expect(rows).toHaveLength(1);
    expect(kindOf(rows[0])).toBe("review");
    expect(search()).toContain("kind=review");

    // Back to all: the default is the ABSENCE of the param, not a third value.
    fireEvent.change(screen.getByLabelText("filter by kind"), { target: { value: "all" } });
    expect(await screen.findAllByTestId("run-row")).toHaveLength(3);
    expect(search()).not.toContain("kind=");
  });

  it("starts on the kind the URL already carries", async () => {
    renderRuns("/workspaces/ws-1?section=runs&kind=review");
    const rows = await screen.findAllByTestId("run-row");
    expect(rows).toHaveLength(1);
    expect(kindOf(rows[0])).toBe("review");
    expect((screen.getByLabelText("filter by kind") as HTMLSelectElement).value).toBe("review");
  });

  it("narrows by state, across both kinds", async () => {
    renderRuns();
    await screen.findAllByTestId("run-row");
    fireEvent.change(screen.getByLabelText("filter by state"), { target: { value: "completed" } });
    const rows = await screen.findAllByTestId("run-row");
    expect(rows).toHaveLength(2);
    expect(rows.map(kindOf).sort()).toEqual(["build", "review"]);
  });

  it("says so when the two filters agree on nothing", async () => {
    renderRuns();
    await screen.findAllByTestId("run-row");
    fireEvent.change(screen.getByLabelText("filter by kind"), { target: { value: "review" } });
    fireEvent.change(screen.getByLabelText("filter by state"), { target: { value: "running" } });
    expect(screen.queryAllByTestId("run-row")).toHaveLength(0);
    expect(isVisible(await screen.findByText(/No run matches this filter/i))).toBe(true);
  });
});

describe("the list is live (AC-8)", () => {
  it("takes in a run raised while it is open, on the same keys as before", async () => {
    state.reviews = [review()];
    const qc = renderRuns();
    expect(await screen.findAllByTestId("run-row")).toHaveLength(1);

    state.builds = [build({ created_at: "2026-08-08T11:00:00Z" })];
    await jobChanged(qc);

    const rows = await screen.findAllByTestId("run-row");
    expect(rows).toHaveLength(2);
    expect(kindOf(rows[0])).toBe("build");
  });

  it("stops filtering by a state nothing is in any more", async () => {
    // Otherwise finishing the one running build empties the list under a
    // control still reading "running" — a list that looks broken because of a
    // filter the runs outgrew.
    state.builds = [build({ state: "running" })];
    state.reviews = [review()];
    const qc = renderRuns();
    await screen.findAllByTestId("run-row");
    fireEvent.change(screen.getByLabelText("filter by state"), { target: { value: "running" } });
    expect(screen.getAllByTestId("run-row")).toHaveLength(1);

    state.builds = [build({ state: "completed" })];
    await jobChanged(qc);

    expect(screen.getAllByTestId("run-row")).toHaveLength(2);
    expect((screen.getByLabelText("filter by state") as HTMLSelectElement).value).toBe("all");
  });
});

describe("a repo with no runs", () => {
  it("shows ONE empty state, not one per kind", async () => {
    renderRuns();
    const empty = await screen.findByText(/No run has happened in this repo yet/i);
    expect(isVisible(empty)).toBe(true);
    expect(document.querySelectorAll(".empty")).toHaveLength(1);
    // Nothing to narrow, so nothing offers to narrow it.
    expect(screen.queryByLabelText("filter by kind")).toBeNull();
  });
});

describe("the transcript", () => {
  it("renders the newest run's transcript without being asked", async () => {
    state.reviews = [review()];
    state.transcript = [
      { id: "t1", source: "agent", content: "Reviewed PR #341", at: "2026-08-08T10:01:00Z" },
    ];
    renderRuns();
    // findByText, not findByTestId: the panel exists immediately holding its
    // empty state, so asserting on the container would race the second query.
    expect(await screen.findByText(/Reviewed PR #341/)).toBeTruthy();
  });

  it("has no composer at all — hidden, not disabled", async () => {
    // A spec run's composer is how a human shapes the draft. A managed run is
    // the control plane's work: a greyed box would promise a capability that is
    // switched off, so there must be NO box.
    state.builds = [build()];
    state.transcript = [
      { id: "t1", source: "agent", content: "hello", at: "2026-08-08T10:01:00Z" },
    ];
    renderRuns();
    await screen.findByTestId("run-transcript");
    expect(document.querySelector(".chat-composer")).toBeNull();
    expect(document.querySelector("textarea")).toBeNull();
  });

  it("renders the agent's markdown as markdown, not punctuation", async () => {
    // The e2e's own screenshot showed `**Pass ends with no action…**` as
    // literal asterisks: this mapping had drifted behind the Loop page's.
    state.reviews = [review()];
    state.transcript = [
      {
        id: "t1",
        source: "agent",
        content: "## Verdict\n\n**No action** — already reviewed.",
        at: "2026-08-08T10:01:00Z",
      },
    ];
    renderRuns();
    const strong = await screen.findByText("No action");
    expect(strong.tagName).toBe("STRONG");
    expect(screen.getByText("Verdict").tagName).toBe("H2");
  });

  it("copies the FULL transcript even when the view folds agent activity (MAIN-471)", async () => {
    state.reviews = [review()];
    state.transcript = [
      { id: "l1", job_id: "job-1", source: "system", content: "started", at: "2026-08-08T10:00:00Z" },
      // A tool-activity ladder line the VIEW folds away.
      { id: "l2", job_id: "job-1", source: "agent", content: "· Bash cargo test", at: "2026-08-08T10:00:01Z" },
      { id: "l3", job_id: "job-1", source: "system", content: "verdict: approved", at: "2026-08-08T10:00:02Z" },
    ];
    const writeText = vi.fn(async (_text: string) => {});
    Object.assign(navigator, { clipboard: { writeText } });

    renderRuns();
    fireEvent.click(await screen.findByTestId("transcript-copy"));
    await new Promise((r) => setTimeout(r, 0));
    const copied = writeText.mock.calls[0][0] as string;
    expect(copied).toContain("· Bash cargo test");
    expect(copied).toContain("## agent");
    expect(copied).toContain("verdict: approved");
  });
});

describe("the links the two old sections left behind (AC-6)", () => {
  function renderRedirect(url: string) {
    function Harness() {
      useLegacyRunsSectionRedirect();
      return <Search />;
    }
    return render(
      <MemoryRouter initialEntries={[url]}>
        <Harness />
      </MemoryRouter>,
    );
  }

  it("lands ?section=builds on the runs section with the build kind applied", () => {
    renderRedirect("/workspaces/ws-1?section=builds");
    expect(search()).toBe("?section=runs&kind=build");
  });

  it("lands ?section=reviews on the runs section with the review kind applied", () => {
    renderRedirect("/workspaces/ws-1?section=reviews");
    expect(search()).toBe("?section=runs&kind=review");
  });

  it("leaves every other section alone", () => {
    renderRedirect("/workspaces/ws-1?section=checkouts");
    expect(search()).toBe("?section=checkouts");
  });
});
