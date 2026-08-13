// The repo's ONE runs surface (MAIN-488), carrying the coverage the separate
// Reviews (MAIN-455 AC-5) and Builds (MAIN-461 AC-2) suites held.
//
// What is worth pinning: a run is READ, not driven; two runs of the same PR are
// distinguishable; a build names its card and falls back for a keyless one; and
// — new here — the two kinds share one list, one empty state and one filter,
// with the kind in the URL so an old link still lands somewhere true.
import React from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter, useLocation } from "react-router-dom";

const state = vi.hoisted(() => ({
  builds: [] as unknown[],
  reviews: [] as unknown[],
  transcript: [] as unknown[],
  /** Every cancel/rerun the panel actually sent, in order. */
  posts: [] as { path: string; id?: string }[],
  /** What the next POST refuses with, in the server's own body shape. */
  postError: null as string | null,
  /** The repo, which is where a review row's PR link comes from. */
  workspace: null as unknown,
  /** What both listings refuse with, when they are made to refuse (MAIN-560
   *  AC-5). Cleared to let a retry succeed. */
  listError: null as string | null,
  /** Run ids the job endpoint refuses — how a run the URL names is made to be
   *  gone or unreadable (MAIN-560 AC-5). */
  goneJobs: [] as string[],
  /** Every page either listing was asked for, in order (MAIN-560 AC-1). */
  pages: [] as { list: string; after?: string; limit?: number }[],
  /** How long a listing takes to answer. Zero for every test but the one about
   *  what the list shows WHILE a page is in flight. */
  listDelay: 0,
}));

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (
      path: string,
      opts?: { params?: { query?: Record<string, unknown>; path?: { id?: string } } },
    ) => {
      // Before the `/builds` arm: the repo itself, which the panel reads for
      // the remote a review's PR link is built from.
      if (path === "/api/v1/workspaces/{id}") return { data: state.workspace };
      // Both run listings answer the pagination contract's envelope
      // (MAIN-557), and PAGE — a slice plus an opaque token that is non-null
      // exactly when the slice came back FULL, which is the server's own rule.
      // The token here is the offset, stringified: the client is forbidden to
      // parse it (MAIN-557), so its shape is this fake's business.
      if (path.includes("/builds") || path.includes("/reviews")) {
        const list = path.includes("/builds") ? "builds" : "reviews";
        const q = opts?.params?.query ?? {};
        state.pages.push({
          list,
          after: q.after as string | undefined,
          limit: q.limit as number | undefined,
        });
        if (state.listDelay) await new Promise((r) => setTimeout(r, state.listDelay));
        if (state.listError) return { error: { error: state.listError } };
        const limit = Number(q.limit ?? 50);
        const from = Number(q.after ?? 0);
        const rows = (list === "builds" ? state.builds : state.reviews).slice(from, from + limit);
        return {
          data: { rows, next_cursor: rows.length === limit ? String(from + limit) : null },
        };
      }
      // Before the `/jobs/` arm: a run's command list is a different endpoint
      // under the same prefix, and it answers a LIST (MAIN-530).
      if (path.endsWith("/commands")) return { data: [] };
      if (path.includes("/jobs/")) {
        if (state.goneJobs.includes(opts?.params?.path?.id ?? "")) {
          return { error: { error: "job not found" } };
        }
        return { data: { transcript: state.transcript } };
      }
      return { data: null };
    }),
    POST: vi.fn(async (path: string, opts?: { params?: { path?: { id?: string } } }) => {
      state.posts.push({ path, id: opts?.params?.path?.id });
      return state.postError ? { error: { error: state.postError } } : { data: {} };
    }),
  },
}));

import { ContextMenuProvider } from "./contextMenu";
import { DialogHost } from "./dialogs";
import { KIND_CHOICES, parseKind } from "./runsFilter";
import {
  mergeRuns,
  pillTone,
  queuedReason,
  reviewMeta,
  rowSecondary,
  runAge,
  runLabel,
  RUNS_MIN_PANE_PX,
  shortHead,
  stateGlyph,
  prWebUrl,
  runHref,
  runStateMeta,
  shownState,
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
  state.posts = [];
  state.postError = null;
  state.workspace = null;
  state.listError = null;
  state.goneJobs = [];
  state.pages = [];
  state.listDelay = 0;
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
  const loc = useLocation();
  return (
    <>
      <span data-testid="search">{loc.search}</span>
      <span data-testid="path">{loc.pathname}</span>
    </>
  );
}

/** The panel in the shape the app mounts it in: the shared context menu and the
 *  dialog host are what its row actions open (MAIN-559), and a panel tested
 *  without them would be testing a component the app never renders. */
function renderRuns(url = "/workspaces/ws-1") {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <MemoryRouter initialEntries={[url]}>
      <QueryClientProvider client={qc}>
        <ContextMenuProvider>
          <WorkspaceRuns workspaceId="ws-1" />
          <Search />
          <DialogHost />
        </ContextMenuProvider>
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
const path = () => screen.getByTestId("path").textContent;
const kindOf = (row: HTMLElement) => row.getAttribute("data-kind");

/** The segmented kind selector (MAIN-556 AC-7), which replaced the detached
 *  `<select>` these tests used to drive. */
const kindSegment = (label: string) => screen.getByRole("radio", { name: label });
const pickKind = (label: string) => fireEvent.click(kindSegment(label));
const chosenKind = () =>
  screen.getAllByRole("radio").find((b) => b.getAttribute("aria-checked") === "true")
    ?.textContent;

/** The toolbar's search field (MAIN-558 AC-1). */
const searchBox = () => screen.getByLabelText("search runs") as HTMLInputElement;
const typeSearch = (q: string) => fireEvent.change(searchBox(), { target: { value: q } });

/** Open the filter popover (AC-3). Its own act, because the count and the chips
 *  outside it are what a closed popover is supposed to still show. */
async function openFilters() {
  // Idempotent: the trigger TOGGLES, so a second click would shut what the
  // caller asked to have open.
  if (!screen.queryByRole("dialog", { name: "run filters" }))
    fireEvent.click(screen.getByTestId("run-filters"));
  return screen.findByRole("dialog", { name: "run filters" });
}

/** Toggle one state inside the popover, by the word the loop shows for it. */
async function pickState(label: string) {
  const panel = await openFilters();
  fireEvent.click(within(panel).getByRole("button", { name: label }));
}

/** Every active-filter chip, as text — the `×` included, since it is what makes
 *  each one individually removable (AC-5). */
const chipTexts = () =>
  screen.queryByTestId("run-filter-chips")
    ? [...screen.getByTestId("run-filter-chips").querySelectorAll(".filter-chip")].map(
        (c) => c.textContent,
      )
    : [];

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

  it("says when a verdict was the control plane's rather than a review", () => {
    // MAIN-516 records a `changes_requested` for a PR that conflicts with its
    // base, MAIN-542 for one the merge queue ejected, so the repair queue can
    // see them. Nobody reviewed either head, and the two causes are not each
    // other — a row must not read as if somebody had reviewed it, nor as the
    // wrong reason.
    const conflicted = review({ review_verdict_source: "conflict" });
    expect(reviewMeta(conflicted as never)).toBe("abcdef1 · conflict, not reviewed");
    const ejected = review({ review_verdict_source: "queue_ejection" });
    expect(reviewMeta(ejected as never)).toBe("abcdef1 · queue ejection, not reviewed");
    expect(reviewMeta(review() as never)).toBe("abcdef1");
    // A source this build does not know renders as a plain review row.
    expect(reviewMeta(review({ review_verdict_source: "martian" }) as never)).toBe("abcdef1");
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

  it("offers search, kind and the rest, visibly, with none applied (MAIN-558)", async () => {
    state.builds = [build()];
    state.reviews = [review()];
    renderRuns();
    await screen.findAllByTestId("run-row");
    expect(isVisible(screen.getByRole("radiogroup", { name: "filter by kind" }))).toBe(true);
    expect(isVisible(searchBox())).toBe(true);
    expect(isVisible(screen.getByTestId("run-filters"))).toBe(true);
    expect(chosenKind()).toBe("All");
    expect(searchBox().value).toBe("");
    // No filter is on, so there is no count and no chip row to read.
    expect(screen.queryByTestId("run-filter-count")).toBeNull();
    expect(screen.queryByTestId("run-filter-chips")).toBeNull();
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
    pickKind("Builds");
    let rows = await screen.findAllByTestId("run-row");
    expect(rows).toHaveLength(2);
    expect(rows.every((r) => kindOf(r) === "build")).toBe(true);
    expect(search()).toContain("kind=build");

    pickKind("Reviews");
    rows = await screen.findAllByTestId("run-row");
    expect(rows).toHaveLength(1);
    expect(kindOf(rows[0])).toBe("review");
    expect(search()).toContain("kind=review");

    // Back to all: the default is the ABSENCE of the param, not a third value.
    pickKind("All");
    expect(await screen.findAllByTestId("run-row")).toHaveLength(3);
    expect(search()).not.toContain("kind=");
  });

  it("starts on the kind the URL already carries", async () => {
    renderRuns("/workspaces/ws-1?section=runs&kind=review");
    const rows = await screen.findAllByTestId("run-row");
    expect(rows).toHaveLength(1);
    expect(kindOf(rows[0])).toBe("review");
    expect(chosenKind()).toBe("Reviews");
  });

  it("narrows by state, across both kinds", async () => {
    renderRuns();
    await screen.findAllByTestId("run-row");
    await pickState("done");
    const rows = await screen.findAllByTestId("run-row");
    expect(rows).toHaveLength(2);
    expect(rows.map(kindOf).sort()).toEqual(["build", "review"]);
  });

  it("names the filters that emptied the list, and offers them back (MAIN-560 AC-6)", async () => {
    renderRuns();
    await screen.findAllByTestId("run-row");
    pickKind("Reviews");
    await pickState("running");
    expect(screen.queryAllByTestId("run-row")).toHaveLength(0);

    const said = await screen.findByTestId("runs-no-filters");
    expect(isVisible(said)).toBe(true);
    // Both narrowings, in the chip row's own words — the KIND included, which
    // has no chip of its own (MAIN-558 AC-5) and is still a reason the list is
    // empty. And not one word that could be read as "this repo has never run
    // anything".
    expect(said.textContent).toContain("Reviews");
    expect(said.textContent).toContain("running");
    expect(said.textContent).toContain("This repo has runs");
    expect(screen.queryByTestId("runs-empty")).toBeNull();

    // Wider than the chip row's `Clear all`, which keeps the kind: this is the
    // way back to the unfiltered list (AC-6).
    fireEvent.click(screen.getByTestId("runs-clear-filters"));
    await waitFor(() => expect(screen.getAllByTestId("run-row")).toHaveLength(3));
    expect(chosenKind()).toBe("All");
    expect(chipTexts()).toEqual([]);
  });
});

describe("search, filters and URL state (MAIN-558)", () => {
  /** Three runs that between them carry every field search matches on. */
  const populated = () => {
    state.builds = [
      build({
        id: "019f8000-0000-7000-8000-00000000b001",
        task_key: "MAIN-512",
        state: "running",
        created_at: "2026-08-13T11:00:00Z",
        branch: "main-512-a-slug",
        initiator: "Ryan Hein",
        commit_sha: "abcdef1234567890abcdef1234567890abcdef12",
      }),
      build({
        id: "019f8000-0000-7000-8000-00000000b002",
        task_key: "MAIN-600",
        state: "completed",
        created_at: "2026-08-13T09:00:00Z",
        branch: "main-600-other",
        initiator: "Dana",
      }),
    ];
    state.reviews = [
      review({
        id: "019f8000-0000-7000-8000-00000000r001",
        review_pr_number: 439,
        review_head_sha: "999888777666555444333222111000fedcba9876",
        state: "queued",
        created_at: "2026-08-13T10:00:00Z",
        initiator: "the converger",
      }),
    ];
  };

  const labels = () =>
    screen.queryAllByTestId("run-row").map((r) => r.querySelector(".runs-row-id")?.textContent);

  it("filters as it is typed, with no submit (AC-1)", async () => {
    populated();
    renderRuns();
    await screen.findAllByTestId("run-row");
    expect(searchBox().placeholder).toBe("Search runs…");

    // Every keystroke narrows: no Enter, no debounce to wait out.
    typeSearch("MAIN-5");
    expect(labels()).toEqual(["MAIN-512"]);
    typeSearch("MAIN-51");
    expect(labels()).toEqual(["MAIN-512"]);
    typeSearch("");
    expect(labels()).toHaveLength(3);
  });

  it("matches every field a row has, and is unbothered by the ones it has not (AC-2)", async () => {
    populated();
    renderRuns();
    await screen.findAllByTestId("run-row");
    for (const [term, expected] of [
      ["MAIN-512", ["MAIN-512"]],
      ["PR #439", ["PR #439"]],
      ["439", ["PR #439"]],
      ["00000000b002", ["MAIN-600"]],
      ["abcdef1", ["MAIN-512"]],
      ["abcdef1234567890abcdef1234567890abcdef12", ["MAIN-512"]],
      ["main-600-other", ["MAIN-600"]],
      ["Dana", ["MAIN-600"]],
      ["converger", ["PR #439"]],
      // A review has no card and no branch, and the build with no commit has
      // no sha: none of that is an error, it is simply not a match.
      ["MAIN-999", []],
    ] as const) {
      typeSearch(term);
      expect(labels(), term).toEqual([...expected]);
    }
  });

  it("counts the active filters on the button and chips them beneath (AC-4, AC-5)", async () => {
    populated();
    renderRuns();
    await screen.findAllByTestId("run-row");
    await pickState("running");
    await pickState("queued");

    expect(screen.getByTestId("run-filter-count").textContent).toBe("2");
    expect(chipTexts()).toEqual(["queued×", "running×"]);
    expect(labels()).toEqual(["MAIN-512", "PR #439"]);
  });

  it("removes one chip and leaves the other (AC-5)", async () => {
    populated();
    renderRuns();
    await screen.findAllByTestId("run-row");
    await pickState("running");
    await pickState("queued");

    fireEvent.click(screen.getByRole("button", { name: "remove filter: running" }));
    expect(chipTexts()).toEqual(["queued×"]);
    expect(screen.getByTestId("run-filter-count").textContent).toBe("1");
    expect(labels()).toEqual(["PR #439"]);
  });

  it("offers Clear all only past one filter, and then clears them all (AC-5)", async () => {
    populated();
    renderRuns();
    await screen.findAllByTestId("run-row");

    await pickState("running");
    // One chip: the button beside it would do exactly what the chip does.
    expect(screen.queryByTestId("run-filters-clear")).toBeNull();

    await pickState("queued");
    expect(isVisible(screen.getByTestId("run-filters-clear"))).toBe(true);

    fireEvent.click(screen.getByTestId("run-filters-clear"));
    expect(chipTexts()).toEqual([]);
    expect(screen.queryByTestId("run-filter-count")).toBeNull();
    expect(labels()).toHaveLength(3);
  });

  it("keeps the kind, the search and the filters independent (AC-6)", async () => {
    populated();
    renderRuns();
    await screen.findAllByTestId("run-row");

    typeSearch("MAIN");
    await pickState("running");
    expect(labels()).toEqual(["MAIN-512"]);

    // Changing the kind preserves both...
    pickKind("Builds");
    expect(searchBox().value).toBe("MAIN");
    expect(chipTexts()).toEqual(["running×"]);
    expect(labels()).toEqual(["MAIN-512"]);

    // ...and changing the search preserves the kind and the filter.
    typeSearch("MAIN-600");
    expect(chosenKind()).toBe("Builds");
    expect(chipTexts()).toEqual(["running×"]);
    expect(labels()).toEqual([]);
  });

  it("puts every dimension in the URL (AC-7)", async () => {
    populated();
    renderRuns();
    await screen.findAllByTestId("run-row");

    typeSearch("MAIN");
    pickKind("Builds");
    await pickState("running");

    const url = new URLSearchParams(search()!);
    expect(url.get("q")).toBe("MAIN");
    expect(url.get("kind")).toBe("build");
    expect(url.get("state")).toBe("running");
  });

  it("restores a filtered view exactly from the URL it was copied as (AC-7)", async () => {
    populated();
    renderRuns("/workspaces/ws-1?section=runs&kind=build&q=MAIN&state=running&initiator=Ryan+Hein");
    await screen.findAllByTestId("run-row");

    expect(chosenKind()).toBe("Builds");
    expect(searchBox().value).toBe("MAIN");
    expect(chipTexts()).toEqual(["running×", "Ryan Hein×"]);
    expect(screen.getByTestId("run-filter-count").textContent).toBe("2");
    expect(labels()).toEqual(["MAIN-512"]);
  });

  it("offers the initiators and branches this repo actually has (AC-3)", async () => {
    populated();
    renderRuns();
    await screen.findAllByTestId("run-row");
    const panel = await openFilters();

    const people = within(panel).getByLabelText("initiator") as HTMLSelectElement;
    expect([...people.options].map((o) => o.textContent)).toEqual([
      "anyone",
      "Dana",
      "Ryan Hein",
      "the converger",
    ]);
    const branch = within(panel).getByLabelText("branch") as HTMLSelectElement;
    expect([...branch.options].map((o) => o.textContent)).toEqual([
      "any branch",
      "main-512-a-slug",
      "main-600-other",
    ]);

    fireEvent.change(branch, { target: { value: "main-600-other" } });
    expect(labels()).toEqual(["MAIN-600"]);
    expect(chipTexts()).toEqual(["main-600-other×"]);
  });

  it("narrows by a raised-range, relative or dated (AC-3)", async () => {
    populated();
    renderRuns();
    await screen.findAllByTestId("run-row");
    const panel = await openFilters();

    fireEvent.change(within(panel).getByLabelText("raised after"), {
      target: { value: "2026-08-13" },
    });
    expect(chipTexts()).toEqual(["from 2026-08-13×"]);
    expect(labels()).toHaveLength(3);

    // A preset and a pair of dates are one dimension: choosing either clears
    // the other, so the chip row never shows two answers to one question.
    fireEvent.click(within(panel).getByRole("button", { name: "last 7 days" }));
    expect(chipTexts()).toEqual(["last 7 days×"]);
    expect((within(panel).getByLabelText("raised after") as HTMLInputElement).value).toBe("");
  });

  it("keeps filters, the selection and the list itself across a live update (AC-8)", async () => {
    populated();
    const qc = renderRuns();
    await screen.findAllByTestId("run-row");

    typeSearch("MAIN");
    await pickState("running");
    fireEvent.click(screen.getAllByTestId("run-row")[0]);
    expect(new URLSearchParams(search()!).get("run")).toBe(
      "019f8000-0000-7000-8000-00000000b001",
    );
    // The scrolling box itself, not its offset: jsdom runs no layout, so what
    // is provable here is that React keeps the same node — which is exactly
    // what keeps its `scrollTop` in a browser. A remount is the only way a
    // repaint loses a scroll position.
    const list = document.querySelector(".runs-list");

    state.builds = [...(state.builds as Record<string, unknown>[]), build({ id: "job-new" })];
    await jobChanged(qc);
    await waitFor(() => expect(screen.getAllByTestId("run-row").length).toBeGreaterThan(0));

    expect(searchBox().value).toBe("MAIN");
    expect(chipTexts()).toEqual(["running×"]);
    expect(new URLSearchParams(search()!).get("run")).toBe(
      "019f8000-0000-7000-8000-00000000b001",
    );
    expect(document.querySelector(".runs-list")).toBe(list);
  });
});

describe("why a queued run is waiting (MAIN-494)", () => {
  const waiting = (over: Record<string, unknown> = {}) =>
    build({
      id: "job-q1",
      state: "queued",
      queued_reason:
        "waiting for node builder-2, which holds this card's worktree. Prune the worktree from the card to release it.",
      queued_reason_kind: { kind: "pinned_node_unavailable", node_name: "builder-2" },
      ...over,
    });

  it("explains a queued run in the panel, so nobody needs a terminal", async () => {
    state.builds = [waiting()];
    renderRuns();
    const reason = await screen.findByTestId("run-reason");
    expect(isVisible(reason)).toBe(true);
    expect(reason.textContent).toContain("builder-2");
    expect(reason.textContent).toContain("Prune the worktree");
  });

  it("carries the gate as a value, so a client never matches on the sentence", async () => {
    state.builds = [waiting()];
    renderRuns();
    const rows = await screen.findAllByTestId("run-row");
    expect(rows[0].getAttribute("data-reason-kind")).toBe("pinned_node_unavailable");
  });

  it("says nothing on a run that is not waiting", async () => {
    // The claim clears both columns; a reason still on screen beside `running`
    // would read as the run being stuck.
    state.builds = [build({ state: "running" })];
    renderRuns();
    await screen.findAllByTestId("run-row");
    expect(screen.queryByTestId("run-reason")).toBeNull();
  });

  it("renders a legacy row's text verbatim, with no gate to go with it", async () => {
    // A row written before the typed column: the sentence is all there is, and
    // parsing it into a cause would be a guess (AC-6).
    state.builds = [
      waiting({
        queued_reason: "no eligible executor: you have no node online",
        queued_reason_kind: null,
      }),
    ];
    renderRuns();
    const reason = await screen.findByTestId("run-reason");
    expect(reason.textContent).toBe("no eligible executor: you have no node online");
    expect(screen.getAllByTestId("run-row")[0].getAttribute("data-reason-kind")).toBeNull();
  });

  it("shows a reason only while queued", () => {
    expect(queuedReason("queued", "at capacity")).toBe("at capacity");
    expect(queuedReason("running", "at capacity")).toBe("");
    expect(queuedReason("queued", null)).toBe("");
  });
});

describe("the list is live (AC-8)", () => {
  it("takes in a run raised while it is open, on the same keys as before", async () => {
    state.reviews = [review()];
    const qc = renderRuns();
    expect(await screen.findAllByTestId("run-row")).toHaveLength(1);

    state.builds = [build({ created_at: "2026-08-08T11:00:00Z" })];
    await jobChanged(qc);

    // `waitFor` the COUNT, not `findAll`: the latter resolves on the first row
    // to appear, which since MAIN-559 is not necessarily the whole list — the
    // panel commits the header and its own queries alongside it.
    await waitFor(() => expect(screen.getAllByTestId("run-row")).toHaveLength(2));
    expect(kindOf(screen.getAllByTestId("run-row")[0])).toBe("build");
  });

  it("keeps a state filter the runs have left, rather than withdrawing it", async () => {
    // The state list is the LOOP's vocabulary now (MAIN-558 AC-3), not a
    // reading of what is on screen — so a run leaving `running` empties the
    // list under a filter that is still, visibly, "running". Withdrawing it
    // instead would silently un-narrow a URL somebody shared.
    state.builds = [build({ state: "running" })];
    state.reviews = [review()];
    const qc = renderRuns();
    await screen.findAllByTestId("run-row");
    await pickState("running");
    expect(screen.getAllByTestId("run-row")).toHaveLength(1);

    state.builds = [build({ state: "completed" })];
    await jobChanged(qc);

    await waitFor(() => expect(screen.queryAllByTestId("run-row")).toHaveLength(0));
    expect(chipTexts()).toEqual(["running×"]);
    expect(search()).toContain("state=running");
  });
});

describe("scrolling the whole history (MAIN-560)", () => {
  /** `n` builds and `n` reviews, interleaved an hour apart, so neither walk can
   *  be paged without the other and the frontier is exercised for real. */
  function history(n: number) {
    state.builds = Array.from({ length: n }, (_, i) =>
      build({
        id: `b${String(i).padStart(3, "0")}`,
        task_key: `MAIN-${900 - i}`,
        created_at: new Date(Date.UTC(2026, 7, 8, 0, 0, 0) - i * 7200_000).toISOString(),
      }),
    );
    state.reviews = Array.from({ length: n }, (_, i) =>
      review({
        id: `r${String(i).padStart(3, "0")}`,
        review_pr_number: 900 - i,
        created_at: new Date(Date.UTC(2026, 7, 8, 1, 0, 0) - i * 7200_000).toISOString(),
      }),
    );
  }

  const rowIds = () =>
    screen.queryAllByTestId("run-row").map((r) => r.getAttribute("data-run-id") as string);

  /** Keep pressing `load more` until the list states its end (AC-1, AC-4). */
  async function walkToTheEnd() {
    for (let guard = 0; guard < 30; guard += 1) {
      if (screen.queryByTestId("runs-end")) return;
      const more = screen.queryByTestId("runs-load-more");
      if (!more) throw new Error("neither more to load nor an end to the history");
      fireEvent.click(more);
      await waitFor(() => expect(screen.queryByTestId("runs-loading-more")).toBeNull());
    }
    throw new Error("the walk never ended");
  }

  it("walks the whole set with no duplicate and no gap (AC-1)", async () => {
    history(120);
    renderRuns();
    await screen.findAllByTestId("run-row");
    // A cap would make an old run unreachable, which is the thing AC-1 forbids
    // — so the first page really is a page, not the lot.
    expect(rowIds().length).toBeLessThan(240);

    await walkToTheEnd();

    const seen = rowIds();
    // No duplicate: the count and the set agree. No gap: every run, once.
    expect(new Set(seen).size).toBe(seen.length);
    expect(seen.length).toBe(240);
    // And newest first the whole way down — across both kinds and across every
    // page boundary either walk crossed. The reviews are an hour newer than the
    // builds they interleave with, so this order is the merge doing its job and
    // not two lists concatenated.
    const expected = Array.from({ length: 120 }, (_, i) => [
      `r${String(i).padStart(3, "0")}`,
      `b${String(i).padStart(3, "0")}`,
    ]).flat();
    expect(seen).toEqual(expected);
  });

  it("asks each listing for a page, passing the cursor back verbatim (AC-1)", async () => {
    history(120);
    renderRuns();
    await screen.findAllByTestId("run-row");
    await walkToTheEnd();

    const builds = state.pages.filter((p) => p.list === "builds");
    expect(builds[0]).toMatchObject({ after: undefined, limit: 50 });
    // The token the fake handed back, unparsed and unmodified.
    expect(builds.map((p) => p.after)).toContain("50");
  });

  it("states the end of history exactly once, and only at the end (AC-4, AC-8)", async () => {
    history(60);
    renderRuns();
    await screen.findAllByTestId("run-row");
    expect(screen.queryByTestId("runs-end")).toBeNull();

    await walkToTheEnd();
    expect(screen.getAllByTestId("runs-end")).toHaveLength(1);
    expect(screen.queryByTestId("runs-load-more")).toBeNull();
  });

  it("says so at the end of a short history, with nothing to load", async () => {
    state.builds = [build()];
    state.reviews = [review()];
    renderRuns();
    await screen.findAllByTestId("run-row");
    expect(await screen.findByTestId("runs-end")).toBeTruthy();
    expect(screen.queryByTestId("runs-load-more")).toBeNull();
  });

  it("shows an inline loading row and keeps the scroll where it was (AC-2)", async () => {
    history(120);
    renderRuns();
    await screen.findAllByTestId("run-row");

    // The scrolling box itself, not its offset: jsdom runs no layout, so what
    // is provable here is that React keeps the same node and the same rows
    // above the new ones — which is exactly what keeps `scrollTop` in a
    // browser. A remount is the only way a repaint loses a scroll position.
    const box = document.querySelector(".runs-list");
    const before = rowIds();

    // Slow enough that "while it is loading" is a state a test can be in.
    state.listDelay = 40;
    fireEvent.click(screen.getByTestId("runs-load-more"));
    // The affordance is a ROW at the bottom, present WHILE the page is in
    // flight, not an overlay or a spinner somewhere else.
    expect(await screen.findByTestId("runs-loading-more")).toBeTruthy();
    // Nothing above it moved to make room for it.
    expect(rowIds().slice(0, before.length)).toEqual(before);
    state.listDelay = 0;
    await waitFor(() => expect(screen.queryByTestId("runs-loading-more")).toBeNull());

    expect(document.querySelector(".runs-list")).toBe(box);
    expect(rowIds().slice(0, before.length)).toEqual(before);
    expect(rowIds().length).toBeGreaterThan(before.length);
  });

  it("pages itself when the bottom of the list comes into view (AC-1)", async () => {
    const observers: { cb: (e: { isIntersecting: boolean }[]) => void }[] = [];
    class FakeObserver {
      constructor(cb: (e: { isIntersecting: boolean }[]) => void) {
        observers.push({ cb });
      }
      observe() {}
      disconnect() {}
    }
    vi.stubGlobal("IntersectionObserver", FakeObserver);
    try {
      history(120);
      renderRuns();
      await screen.findAllByTestId("run-row");
      const before = rowIds().length;

      // What a scroll to the bottom does: nothing is clicked.
      await act(async () => {
        observers[observers.length - 1].cb([{ isIntersecting: true }]);
      });
      await waitFor(() => expect(rowIds().length).toBeGreaterThan(before));
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("takes a run raised mid-scroll without duplicating or reordering (AC-3)", async () => {
    history(120);
    const qc = renderRuns();
    await screen.findAllByTestId("run-row");
    fireEvent.click(screen.getByTestId("runs-load-more"));
    await waitFor(() => expect(screen.queryByTestId("runs-loading-more")).toBeNull());
    fireEvent.click(screen.getAllByTestId("run-row")[3]);
    const chosen = new URLSearchParams(search()!).get("run");
    const box = document.querySelector(".runs-list");
    const before = rowIds();

    // Raised at the top of the list, as a new run always is — which shifts
    // every page boundary underneath it.
    state.builds = [
      build({ id: "b-new", task_key: "MAIN-999", created_at: "2026-08-09T00:00:00Z" }),
      ...(state.builds as Record<string, unknown>[]),
    ];
    await jobChanged(qc);
    await waitFor(() => expect(rowIds()).toContain("b-new"));

    const after = rowIds();
    expect(new Set(after).size).toBe(after.length);
    expect(after[0]).toBe("b-new");
    // Every row that survived is in the order it was already in, and the only
    // rows that can leave are at the TAIL — a run raised at the top pushes the
    // oldest fetched row of its own walk onto a page nobody has asked for yet,
    // which is a row the client no longer holds rather than one it reordered.
    const kept = after.filter((id) => before.includes(id));
    expect(kept).toEqual(before.slice(0, kept.length));
    // And the reader keeps what they had: the same run open, and the same
    // scrolling node (AC-7).
    expect(new URLSearchParams(search()!).get("run")).toBe(chosen);
    expect(document.querySelector(".runs-list")).toBe(box);
  });

  it("keeps the selection and the filters across a page (AC-7)", async () => {
    history(120);
    renderRuns();
    await screen.findAllByTestId("run-row");
    await pickState("running");
    typeSearch("MAIN");
    fireEvent.click(screen.getAllByTestId("run-row")[0]);
    const chosen = new URLSearchParams(search()!).get("run");
    const box = document.querySelector(".runs-list");

    fireEvent.click(screen.getByTestId("runs-load-more"));
    await waitFor(() => expect(screen.queryByTestId("runs-loading-more")).toBeNull());

    expect(new URLSearchParams(search()!).get("run")).toBe(chosen);
    expect(searchBox().value).toBe("MAIN");
    expect(chipTexts()).toEqual(["running×"]);
    expect(document.querySelector(".runs-list")).toBe(box);
  });
});

describe("when the list cannot be loaded (MAIN-560 AC-5)", () => {
  it("says so, in the server's words, rather than showing an empty repo", async () => {
    state.builds = [build()];
    state.listError = "the control plane is unreachable";
    renderRuns();

    const said = await screen.findByTestId("runs-load-failed");
    expect(said.textContent).toContain("the control plane is unreachable");
    // The failure this replaces: `?? []` rendered a dead endpoint as a repo
    // that has never run anything.
    expect(screen.queryByTestId("runs-empty")).toBeNull();
    expect(screen.queryAllByTestId("run-row")).toHaveLength(0);
  });

  it("retries only when asked, and then shows the list (NG-4)", async () => {
    state.builds = [build()];
    state.reviews = [review()];
    state.listError = "the control plane is unreachable";
    renderRuns();
    await screen.findByTestId("runs-load-failed");

    const asked = state.pages.length;
    // Nothing on its own: a retry loop against a control plane that is down is
    // what NG-4 forbids.
    await act(async () => {
      await new Promise((r) => setTimeout(r, 30));
    });
    expect(state.pages.length).toBe(asked);

    state.listError = null;
    fireEvent.click(screen.getByTestId("runs-retry"));
    await waitFor(() => expect(screen.getAllByTestId("run-row")).toHaveLength(2));
  });

  it("keeps the rows it has when a LATER page fails, and offers the retry there", async () => {
    state.builds = Array.from({ length: 60 }, (_, i) =>
      build({
        id: `b${i}`,
        created_at: new Date(Date.UTC(2026, 7, 8) - i * 3600_000).toISOString(),
      }),
    );
    state.reviews = [];
    renderRuns();
    await screen.findAllByTestId("run-row");
    const before = screen.getAllByTestId("run-row").length;

    state.listError = "the control plane is unreachable";
    fireEvent.click(screen.getByTestId("runs-load-more"));

    const failed = await screen.findByTestId("runs-more-failed");
    expect(failed.textContent).toContain("the control plane is unreachable");
    // The rows already read are not taken away to show an error about the ones
    // that were not.
    expect(screen.getAllByTestId("run-row")).toHaveLength(before);
  });
});

describe("the run the URL names is gone (MAIN-560 AC-5)", () => {
  it("says so instead of quietly opening a different run", async () => {
    state.builds = [build()];
    state.reviews = [review()];
    state.goneJobs = ["job-deleted"];
    renderRuns("/workspaces/ws-1?section=runs&run=job-deleted");

    const said = await screen.findByTestId("run-gone");
    expect(isVisible(said)).toBe(true);
    // The LIST is fine — this is about the pane beside it.
    expect(screen.getAllByTestId("run-row")).toHaveLength(2);
    expect(screen.queryByTestId("run-header")).toBeNull();

    fireEvent.click(screen.getByTestId("run-gone-newest"));
    await waitFor(() => expect(screen.queryByTestId("run-gone")).toBeNull());
    expect(new URLSearchParams(search()!).get("run")).toBeNull();
    expect(screen.getByTestId("run-header")).toBeTruthy();
  });

  it("does not call a run gone merely because the list has not paged to it", async () => {
    // The reason this asks the server rather than reading the rows on screen:
    // once the list is paged, "not among the rows" is an ordinary state for a
    // run that is perfectly real.
    state.builds = Array.from({ length: 60 }, (_, i) =>
      build({
        id: `b${i}`,
        created_at: new Date(Date.UTC(2026, 7, 8) - i * 3600_000).toISOString(),
      }),
    );
    state.reviews = [];
    renderRuns("/workspaces/ws-1?section=runs&run=b59");
    await screen.findAllByTestId("run-row");

    expect(
      screen.queryAllByTestId("run-row").map((r) => r.getAttribute("data-run-id")),
    ).not.toContain("b59");
    await waitFor(() => expect(screen.getByTestId("run-header")).toBeTruthy());
    expect(screen.queryByTestId("run-gone")).toBeNull();
  });
});

describe("a search that matched nothing (MAIN-560 AC-5)", () => {
  it("is its own state, not the empty repo and not the filters", async () => {
    state.builds = [build()];
    state.reviews = [review()];
    renderRuns();
    await screen.findAllByTestId("run-row");

    typeSearch("MAIN-999");
    const said = await screen.findByTestId("runs-no-search");
    expect(said.textContent).toContain("MAIN-999");
    expect(screen.queryByTestId("runs-empty")).toBeNull();
    expect(screen.queryByTestId("runs-no-filters")).toBeNull();
    // No filter is on, so nothing offers to clear one.
    expect(screen.queryByTestId("runs-clear-filters")).toBeNull();

    fireEvent.click(screen.getByTestId("runs-clear-search"));
    await waitFor(() => expect(screen.getAllByTestId("run-row")).toHaveLength(2));
    expect(search()).not.toContain("q=");
  });

  it("names the filters as well when a search ran into them too", async () => {
    state.builds = [build()];
    state.reviews = [review()];
    renderRuns();
    await screen.findAllByTestId("run-row");

    await pickState("running");
    typeSearch("MAIN-999");
    expect((await screen.findByTestId("runs-no-search-filters")).textContent).toContain("running");
    expect(screen.getByTestId("runs-clear-filters")).toBeTruthy();
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
    expect(screen.queryByLabelText("search runs")).toBeNull();
    expect(screen.queryByTestId("run-filters")).toBeNull();
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

  it("reads as a transcript, with the folded activity as its own expandable kind (MAIN-499)", async () => {
    state.builds = [build({ state: "completed" })];
    state.transcript = [
      { id: "l1", source: "agent", content: "· Bash cargo test", at: "2026-08-08T10:00:00Z" },
      { id: "l2", source: "agent", content: "· Read src/lib.rs", at: "2026-08-08T10:00:01Z" },
      { id: "l3", source: "agent", content: "all green", at: "2026-08-08T10:40:00Z" },
    ];
    renderRuns();
    await screen.findByText("all green");
    expect(document.querySelector(".chat-log")!.className).toContain("transcript");
    // Forty minutes apart and still one header — chat's window would have put a
    // second "agent" over the prose.
    expect(screen.getAllByText("agent")).toHaveLength(1);
    // And the fold is no longer a dead end: its steps are one click away.
    const fold = screen.getByRole("button", { name: /2 steps/ });
    fireEvent.click(fold);
    expect(screen.getByText("· Bash cargo test")).toBeTruthy();
    expect(screen.getByText("· Read src/lib.rs")).toBeTruthy();
  });

  it("says a live run is working, in the loop view's own words (AC-6)", async () => {
    state.builds = [build({ state: "running" })];
    state.transcript = [
      { id: "t1", source: "agent", content: "reading the card", at: "2026-08-08T10:01:00Z" },
    ];
    renderRuns();
    await screen.findByText("reading the card");
    const typing = document.querySelector(".chat-typing");
    expect(isVisible(typing)).toBe(true);
    expect(typing!.textContent).toContain("the operator agent is working…");
    // The animated part is the shared indicator, not a second one built here.
    expect(document.querySelectorAll(".chat-typing-dots i")).toHaveLength(3);
  });

  it("says a queued run is waiting for an executor, not working", async () => {
    state.builds = [build({ state: "queued" })];
    state.transcript = [
      { id: "t1", source: "system", content: "enqueued", at: "2026-08-08T10:01:00Z" },
    ];
    renderRuns();
    await screen.findByText("enqueued");
    expect(document.querySelector(".chat-typing")!.textContent).toContain(
      "waiting for an executor…",
    );
  });

  it("shows no indicator over a run that has finished", async () => {
    // The indicator is the operator's only cue that the agent is alive; leaving
    // it over a finished run is the specific lie `agentActivityLabel` prevents.
    state.builds = [build({ state: "completed" })];
    state.transcript = [
      { id: "t1", source: "agent", content: "opened PR #12", at: "2026-08-08T10:01:00Z" },
    ];
    renderRuns();
    await screen.findByText("opened PR #12");
    expect(document.querySelector(".chat-typing")).toBeNull();
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

// One stable row shape, and a toolbar that does not scroll away (MAIN-556).
//
// jsdom runs no layout engine, so nothing here can measure a pixel. The split
// is deliberate and it is the whole method: the SHAPE a height guarantee needs
// — same cells, same order, same grid areas, nothing conditional — is asserted
// on the DOM below, and the geometry that turns that shape into equal heights
// is asserted on the stylesheet in `WorkspaceRunsStyles.test.ts`. Either half
// alone would pass while the list still jumped.
describe("one stable row shape (MAIN-556)", () => {
  /** The row's cells, in the order the grid places them. */
  const CELLS = [
    "runs-row-kind",
    "runs-row-id",
    "runs-row-state",
    "runs-row-meta",
    "runs-row-time",
    // Reserved on every row, showing or not (MAIN-559 AC-1) — a row that only
    // grew this cell on hover would reflow at the moment of pointing at it.
    "runs-row-menu",
  ];

  const cellsOf = (row: HTMLElement) =>
    [...row.children].map((c) => CELLS.find((k) => c.classList.contains(k)));

  /** Four rows that between them cover every way content used to change a
   *  row's height: an outcome, a bare review, a queued run with a sentence for
   *  a reason, and a run with nothing to say on line 2 at all. */
  function mixedContent() {
    state.builds = [
      build({ id: "b-out", state: "completed", build_outcome: "pr_opened" }),
      build({ id: "b-bare", state: "running", build_outcome: null, created_at: "2026-08-08T07:00:00Z" }),
      build({
        id: "b-wait",
        state: "queued",
        created_at: "2026-08-08T06:00:00Z",
        queued_reason:
          "waiting for node builder-2, which holds this card's worktree. Prune the worktree from the card to release it.",
        queued_reason_kind: { kind: "pinned_node_unavailable" },
      }),
    ];
    state.reviews = [review()];
  }

  it("gives every row the same cells in the same order, whatever it holds (AC-1, AC-4)", async () => {
    mixedContent();
    renderRuns();
    const rows = await screen.findAllByTestId("run-row");
    expect(rows).toHaveLength(4);
    for (const row of rows) {
      // Not "contains these" — EQUALS. A row that dropped its empty secondary
      // cell would be a row the grid lays out with one line, which is the bug.
      expect(cellsOf(row)).toEqual(CELLS);
    }
  });

  it("keeps the waiting sentence on line 2 instead of growing a third line (AC-1)", async () => {
    mixedContent();
    renderRuns();
    await screen.findAllByTestId("run-row");
    const reason = screen.getByTestId("run-reason");
    // Inside the ONE secondary cell — the old row gave the reason a line of its
    // own, which is precisely what made a queued row taller than its
    // neighbours.
    expect(reason.parentElement!.classList.contains("runs-row-meta")).toBe(true);
  });

  it("shows the whole state word at the width the browser is designed for (AC-2, AC-8)", async () => {
    // The pane width is the SCENARIO; jsdom cannot lay it out. What is asserted
    // here is that the label reaching the DOM is the full one — no abbreviation
    // and no conditional rendering — for the longest state the loop has.
    // `WorkspaceRunsStyles.test.ts` asserts the column reserved to hold it.
    state.builds = [build({ state: "waiting_on_human" })];
    render(
      <MemoryRouter>
        <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
          <ContextMenuProvider>
            <div style={{ width: RUNS_MIN_PANE_PX }}>
              <WorkspaceRuns workspaceId="ws-1" />
            </div>
          </ContextMenuProvider>
        </QueryClientProvider>
      </MemoryRouter>,
    );
    // The ROW's badge: the detail header states the same thing (MAIN-559 AC-7),
    // and it is the row's column this is about.
    const row = (await screen.findAllByTestId("run-row"))[0];
    const badge = within(row).getByRole("img", { name: "state: waiting on human" });
    expect(isVisible(badge)).toBe(true);
    expect(badge.textContent).toContain("waiting on human");
  });

  it("offers the full text of everything that can truncate, on hover (AC-3)", async () => {
    mixedContent();
    renderRuns();
    const rows = await screen.findAllByTestId("run-row");
    for (const row of rows) {
      const id = row.querySelector(".runs-row-id")!;
      expect(id.getAttribute("title")).toBe(id.textContent);
    }
    const waiting = rows.find((r) => r.getAttribute("data-reason-kind"))!;
    expect(waiting.querySelector(".runs-row-meta")!.getAttribute("title")).toContain(
      "Prune the worktree",
    );
    // Nothing to truncate, no tooltip: an empty `title` is a tooltip that pops
    // up saying nothing.
    const bare = rows.find((r) => !r.querySelector(".runs-row-meta")!.textContent)!;
    expect(bare.querySelector(".runs-row-meta")!.getAttribute("title")).toBeNull();
  });

  it("joins the outcome and the waiting sentence into one secondary value", () => {
    expect(rowSecondary({ meta: "pr_opened", reason: "" } as never)).toBe("pr_opened");
    expect(rowSecondary({ meta: "", reason: "at capacity" } as never)).toBe("at capacity");
    expect(rowSecondary({ meta: "abcdef1", reason: "at capacity" } as never)).toBe(
      "abcdef1 · at capacity",
    );
  });

  it("tells the states apart by SHAPE as well as colour (AC-5)", () => {
    // Greyscale is the test: strip the palette and `failed`, `running` and the
    // two waiting states must still be four different marks.
    const states = [
      "queued",
      "claimed",
      "running",
      "waiting_on_human",
      "completed",
      "failed",
      "canceled",
    ];
    const glyphs = states.map(stateGlyph);
    expect(new Set(glyphs).size).toBe(states.length);
    expect(glyphs.every((g) => g.length > 0)).toBe(true);
    // An unknown state still gets a mark rather than a hole in the badge.
    expect(stateGlyph("martian")).toBe("•");
  });

  it("carries the shape into the row, beside the state word", async () => {
    state.builds = [build({ state: "failed" })];
    renderRuns();
    const rows = await screen.findAllByTestId("run-row");
    expect(rows[0].querySelector(".runs-row-glyph")!.textContent).toBe(stateGlyph("failed"));
  });

  it("marks the open run with a class the accent edge hangs off (AC-5)", async () => {
    mixedContent();
    renderRuns();
    const rows = await screen.findAllByTestId("run-row");
    expect(rows.filter((r) => r.classList.contains("is-open"))).toHaveLength(1);
    fireEvent.click(rows[2]);
    const after = screen.getAllByTestId("run-row");
    expect(after[2].classList.contains("is-open")).toBe(true);
    expect(after[0].classList.contains("is-open")).toBe(false);
  });

  it("keeps the toolbar out of the box that scrolls (AC-6)", async () => {
    mixedContent();
    renderRuns();
    await screen.findAllByTestId("run-row");
    const toolbar = document.querySelector(".runs-toolbar")!;
    const list = document.querySelector(".runs-list")!;
    // Not "the toolbar is above the list" — that a stylesheet could still undo.
    // The toolbar is not INSIDE the scroller, so there is no scroll offset that
    // can move it.
    expect(list.contains(toolbar)).toBe(false);
    expect(toolbar.parentElement).toBe(list.parentElement);
    expect(toolbar.parentElement!.classList.contains("runs-browser")).toBe(true);
    // And every row is in the scroller, so scrolling it is what moves them.
    for (const row of screen.getAllByTestId("run-row")) expect(list.contains(row)).toBe(true);
  });

  it("filters from the toolbar without navigating (AC-7)", async () => {
    mixedContent();
    renderRuns("/workspaces/ws-1?section=runs");
    await screen.findAllByTestId("run-row");
    const group = screen.getByRole("radiogroup", { name: "filter by kind" });
    // In the toolbar, not in the panel's upper-right corner.
    expect(document.querySelector(".runs-toolbar")!.contains(group)).toBe(true);
    expect(group.querySelectorAll("a")).toHaveLength(0);

    const before = path();
    pickKind("Reviews");
    expect(screen.getAllByTestId("run-row")).toHaveLength(1);
    expect(path()).toBe(before);
    expect(search()).toBe("?section=runs&kind=review");
  });

  it("names the three segments the card names, wired to the existing parse (AC-7)", () => {
    expect(KIND_CHOICES.map((c) => c.label)).toEqual(["All", "Builds", "Reviews"]);
    expect(KIND_CHOICES.map((c) => c.value)).toEqual(["all", "build", "review"]);
    for (const c of KIND_CHOICES) expect(parseKind(c.value === "all" ? null : c.value)).toBe(c.value);
  });

  it("moves the kind with the arrows, as one tab stop (AC-7)", async () => {
    mixedContent();
    renderRuns();
    await screen.findAllByTestId("run-row");
    const group = screen.getByRole("radiogroup", { name: "filter by kind" });
    const tabbable = screen
      .getAllByRole("radio")
      .filter((b) => b.getAttribute("tabindex") === "0");
    expect(tabbable).toHaveLength(1);
    expect(tabbable[0].textContent).toBe("All");

    fireEvent.keyDown(group, { key: "ArrowRight" });
    expect(chosenKind()).toBe("Builds");
    fireEvent.keyDown(group, { key: "ArrowLeft" });
    expect(chosenKind()).toBe("All");
    // Wraps, so the last segment is one key from the first.
    fireEvent.keyDown(group, { key: "ArrowLeft" });
    expect(chosenKind()).toBe("Reviews");
  });

  it("arrows the selection down the list and opens with Enter (AC-9)", async () => {
    mixedContent();
    renderRuns();
    const rows = await screen.findAllByTestId("run-row");
    const list = screen.getByRole("listbox", { name: "runs" });
    // One tab stop for the whole list, landing on what the pane already shows.
    expect(rows.filter((r) => r.getAttribute("tabindex") === "0")).toHaveLength(1);
    expect(rows[0].getAttribute("tabindex")).toBe("0");
    expect(rows[0].getAttribute("aria-selected")).toBe("true");

    fireEvent.keyDown(list, { key: "ArrowDown" });
    expect(document.activeElement).toBe(screen.getAllByTestId("run-row")[1]);
    // Arrowing MOVES; it does not open. The transcript still belongs to row 0.
    expect(screen.getAllByTestId("run-row")[0].getAttribute("aria-selected")).toBe("true");

    fireEvent.keyDown(list, { key: "Enter" });
    expect(screen.getAllByTestId("run-row")[1].getAttribute("aria-selected")).toBe("true");
    expect(screen.getAllByTestId("run-row")[0].getAttribute("aria-selected")).toBe("false");
  });

  it("stops at the ends of the list rather than wrapping past them (AC-9)", async () => {
    mixedContent();
    renderRuns();
    const rows = await screen.findAllByTestId("run-row");
    const list = screen.getByRole("listbox", { name: "runs" });
    fireEvent.keyDown(list, { key: "ArrowUp" });
    expect(document.activeElement).toBe(rows[0]);
    for (let i = 0; i < rows.length + 2; i++) fireEvent.keyDown(list, { key: "ArrowDown" });
    expect(document.activeElement).toBe(screen.getAllByTestId("run-row")[rows.length - 1]);
  });

  it("names the kind and state badges for a reader who cannot see them (AC-9)", async () => {
    state.builds = [build({ state: "running" })];
    renderRuns();
    const row = (await screen.findAllByTestId("run-row"))[0];
    expect(isVisible(within(row).getByRole("img", { name: "kind: build" }))).toBe(true);
    expect(isVisible(within(row).getByRole("img", { name: "state: running" }))).toBe(true);
  });

  it("ages a run in the queue panel's words, and loses only the age to a bad date", () => {
    const now = Date.parse("2026-08-08T12:00:00Z");
    expect(runAge("2026-08-08T11:55:00Z", now)).toBe("5m");
    expect(runAge("2026-08-07T10:00:00Z", now)).toBe("1d");
    // Clock skew: a run raised "after" now is not negative time.
    expect(runAge("2026-08-08T12:30:00Z", now)).toBe("just now");
    expect(runAge("not a date", now)).toBe("");
  });

  it("puts an age and its exact instant on every row (AC-1)", async () => {
    mixedContent();
    renderRuns();
    const rows = await screen.findAllByTestId("run-row");
    for (const row of rows) {
      const time = row.querySelector(".runs-row-time")!;
      expect(time.textContent).not.toBe("");
      // The relative word is the glance; the timestamp behind it is the answer
      // to "when exactly", without spending a column on it.
      expect(time.getAttribute("title")).toMatch(/^2026-08-0\d/);
    }
  });
});

// ── Row actions (MAIN-559) ─────────────────────────────────────────────────
describe("row actions (MAIN-559)", () => {
  const menu = () => screen.queryByRole("menu");
  /** The LABEL of each row, not its whole text: a refused action carries its
   *  reason in a hint beside the label (AC-6), and folding the two together
   *  would make every assertion here about the reason as well as the action. */
  const menuLabels = () =>
    screen
      .queryAllByRole("menuitem")
      .map((i) => i.querySelector(".ctxmenu-label")?.textContent?.trim() ?? "");
  const menuItem = (name: string) =>
    screen.getAllByRole("menuitem").find((i) => i.textContent?.startsWith(name))!;
  const actionsButton = (label: string) => screen.getByLabelText(`actions for ${label}`);

  /** Both routes to a row's menu, so every assertion below can be made about
   *  each of them (AC-1) rather than about whichever one is convenient. */
  const openBy = {
    rightClick: (row: HTMLElement) => fireEvent.contextMenu(row),
    button: (label: string) => fireEvent.click(actionsButton(label)),
  };

  it("puts the same menu behind right-click AND a visible button (AC-1)", async () => {
    state.builds = [build({ state: "running" })];
    renderRuns();
    const row = (await screen.findAllByTestId("run-row"))[0];

    // The button is on the row, not somewhere the pointer has to find.
    const button = within(row).getByTestId("run-actions");
    expect(row.contains(button)).toBe(true);
    // And it is not a second tab stop: the list stays one, with Shift+F10 as
    // the keyboard's route.
    expect(button.getAttribute("tabindex")).toBe("-1");

    openBy.rightClick(row);
    const viaRightClick = menuLabels();
    expect(viaRightClick).toEqual(["Open", "Cancel run", "Copy run ID", "Copy link"]);
    fireEvent.keyDown(menu()!, { key: "Escape" });

    openBy.button("MAIN-42");
    expect(menuLabels()).toEqual(viaRightClick);
  });

  it("offers what the state permits, and nothing it does not (AC-2)", async () => {
    state.builds = [
      build({ id: "b-live", state: "running", task_key: "MAIN-42" }),
      build({
        id: "b-dead",
        state: "failed",
        task_key: "MAIN-43",
        created_at: "2026-08-08T08:00:00Z",
      }),
    ];
    renderRuns();
    const rows = await screen.findAllByTestId("run-row");

    openBy.rightClick(rows[0]);
    expect(menuLabels()).toContain("Cancel run");
    expect(menuLabels()).not.toContain("Re-run");
    fireEvent.keyDown(menu()!, { key: "Escape" });

    openBy.rightClick(rows[1]);
    expect(menuLabels()).toContain("Re-run");
    // The one thing AC-2 states negatively: never on a terminal run.
    expect(menuLabels()).not.toContain("Cancel run");
  });

  it("confirms a cancel, naming the run and what stops (AC-3)", async () => {
    state.builds = [build({ state: "running" })];
    renderRuns();
    const row = (await screen.findAllByTestId("run-row"))[0];
    openBy.rightClick(row);
    await act(async () => {
      fireEvent.click(menuItem("Cancel run"));
    });

    expect(screen.getByText(/Cancel build run MAIN-42\?/)).toBeTruthy();
    expect(screen.getByText(/agent working on this run will be stopped/)).toBeTruthy();
    // Nothing has been sent while the question is on screen.
    expect(state.posts).toHaveLength(0);

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "cancel run" }));
    });
    expect(state.posts).toEqual([{ path: "/api/v1/jobs/{id}/cancel", id: "job-b1" }]);
  });

  it("sends nothing when the confirmation is declined (AC-3)", async () => {
    state.builds = [build({ state: "running" })];
    renderRuns();
    openBy.rightClick((await screen.findAllByTestId("run-row"))[0]);
    await act(async () => {
      fireEvent.click(menuItem("Cancel run"));
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "cancel" }));
    });
    expect(state.posts).toHaveLength(0);
  });

  it("shows the row canceling while the request is out, and disables a second (AC-4)", async () => {
    state.builds = [build({ state: "running" })];
    // A cancel that does not answer, so the in-flight state can be observed.
    let release: (() => void) | null = null;
    const held = new Promise<void>((r) => {
      release = r;
    });
    const api = (await import("@nookos/api")).api as unknown as {
      POST: ReturnType<typeof vi.fn>;
    };
    api.POST.mockImplementationOnce(async (path: string, opts: never) => {
      state.posts.push({ path, id: (opts as { params: { path: { id: string } } }).params.path.id });
      await held;
      return { data: {} };
    });

    renderRuns();
    openBy.rightClick((await screen.findAllByTestId("run-row"))[0]);
    await act(async () => {
      fireEvent.click(menuItem("Cancel run"));
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "cancel run" }));
    });

    const row = screen.getAllByTestId("run-row")[0];
    expect(row.getAttribute("data-pending")).toBe("cancel");
    expect(within(row).getByRole("img", { name: "state: canceling" })).toBeTruthy();

    // And a second cancel is refused with a reason rather than sent again.
    openBy.rightClick(row);
    const again = menuItem("Cancel run");
    expect(again.hasAttribute("disabled")).toBe(true);
    expect(again.textContent).toContain("already canceling");
    fireEvent.keyDown(menu()!, { key: "Escape" });

    await act(async () => {
      release?.();
      await held;
    });
    expect(state.posts).toHaveLength(1);
  });

  it("surfaces an API failure verbatim and keeps the selection (AC-4)", async () => {
    state.builds = [
      build({ id: "b-1", state: "failed", task_key: "MAIN-42" }),
      build({
        id: "b-2",
        state: "failed",
        task_key: "MAIN-43",
        created_at: "2026-08-08T08:00:00Z",
      }),
    ];
    state.postError = "only a failed or canceled job can be re-run";
    renderRuns();
    const rows = await screen.findAllByTestId("run-row");
    fireEvent.click(rows[1]);
    expect(screen.getAllByTestId("run-row")[1].getAttribute("aria-selected")).toBe("true");

    openBy.rightClick(screen.getAllByTestId("run-row")[1]);
    await act(async () => {
      fireEvent.click(menuItem("Re-run"));
    });

    // The server's sentence, not a replacement for it.
    expect(screen.getByTestId("run-failure").textContent).toContain(
      "only a failed or canceled job can be re-run",
    );
    // The run that was acted on is still the open one.
    expect(screen.getAllByTestId("run-row")[1].getAttribute("aria-selected")).toBe("true");
  });

  it("drops a stale action from a menu that is already open (AC-5)", async () => {
    state.builds = [build({ state: "running" })];
    const qc = renderRuns();
    openBy.rightClick((await screen.findAllByTestId("run-row"))[0]);
    expect(menuLabels()).toContain("Cancel run");

    // The run finishes underneath the open menu — the case a snapshot taken at
    // right-click time gets wrong, and the only one this card calls out.
    state.builds = [build({ state: "completed" })];
    await jobChanged(qc);

    await waitFor(() =>
      expect(
        screen.getAllByTestId("run-row")[0].querySelector(".runs-row-state")?.getAttribute("aria-label"),
      ).toBe("state: done"),
    );
    expect(menu()).toBeTruthy();
    expect(menuLabels()).not.toContain("Cancel run");
    expect(menuLabels()).toContain("Re-run");
  });

  it("refuses a cancel chosen for a run that finished during the confirmation (AC-5)", async () => {
    state.builds = [build({ state: "running" })];
    const qc = renderRuns();
    openBy.rightClick((await screen.findAllByTestId("run-row"))[0]);
    await act(async () => {
      fireEvent.click(menuItem("Cancel run"));
    });

    // The dialog is open for as long as somebody takes to read it.
    state.builds = [build({ state: "completed" })];
    await jobChanged(qc);
    await waitFor(() => expect(screen.getByTestId("run-header").textContent).toContain("done"));
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "cancel run" }));
    });

    expect(state.posts).toHaveLength(0);
    expect(screen.getByTestId("run-failure").textContent).toContain("already finished");
  });

  it("keeps a refused re-run visible with its reason (AC-6)", async () => {
    state.builds = [build({ state: "completed" })];
    renderRuns();
    openBy.rightClick((await screen.findAllByTestId("run-row"))[0]);
    const rerun = menuItem("Re-run");
    expect(rerun.hasAttribute("disabled")).toBe(true);
    expect(rerun.textContent).toContain("only a failed or canceled run can be re-run");
  });

  it("opens, navigates and closes the menu from the keyboard (AC-8)", async () => {
    state.builds = [build({ state: "running" })];
    renderRuns();
    const row = (await screen.findAllByTestId("run-row"))[0];
    row.focus();

    fireEvent.keyDown(document, { key: "F10", shiftKey: true });
    expect(menu()).toBeTruthy();
    fireEvent.keyDown(menu()!, { key: "ArrowDown" });
    expect(document.activeElement?.textContent).toContain("Open");
    fireEvent.keyDown(menu()!, { key: "ArrowDown" });
    expect(document.activeElement?.textContent).toContain("Cancel run");
    fireEvent.keyDown(document.activeElement!, { key: "Escape" });
    expect(menu()).toBeNull();
    // Back where it came from, which is a node that still exists.
    expect(document.activeElement).toBe(screen.getAllByTestId("run-row")[0]);

    // The ContextMenu key is the other half of AC-8.
    fireEvent.keyDown(document, { key: "ContextMenu" });
    expect(menu()).toBeTruthy();
  });

  it("lands focus on the row after a cancel, never on a node that has gone (AC-8)", async () => {
    state.builds = [build({ state: "running" })];
    renderRuns();
    openBy.rightClick((await screen.findAllByTestId("run-row"))[0]);
    await act(async () => {
      fireEvent.click(menuItem("Cancel run"));
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "cancel run" }));
    });
    expect(document.activeElement).toBe(screen.getAllByTestId("run-row")[0]);
    expect(document.body.contains(document.activeElement)).toBe(true);
  });

  it("reaches the card from a terminal build row, off the field the API sends (AC-2)", async () => {
    // The plumbing, not the derivation. `runActions` was already proved to add
    // `view-task` when handed a href; what this pins is that the panel HAS one
    // to hand it. `WorkspaceBuildRun` sends `task_key` and no uuid, so a row
    // reading `target_task_id` gets `undefined` and the action silently never
    // appears — which every unit test still passed through.
    state.builds = [build({ state: "completed", task_key: "MAIN-42" })];
    renderRuns();
    openBy.rightClick((await screen.findAllByTestId("run-row"))[0]);
    expect(menuLabels()).toContain("View MAIN-42");

    await act(async () => {
      fireEvent.click(menuItem("View MAIN-42"));
    });
    // The KEY is the route parameter: `/loop/:id` resolves keys and uuids
    // alike server-side (MAIN-209), which is what makes the key sufficient.
    expect(path()).toBe("/loop/MAIN-42");
  });

  it("offers no card link on a build row whose card has gone", async () => {
    // `task_key` is nullable — the card was deleted. No link is the honest
    // rendering; a link to nowhere is not.
    state.builds = [build({ state: "completed", task_key: null })];
    renderRuns();
    openBy.rightClick((await screen.findAllByTestId("run-row"))[0]);
    expect(menuLabels().some((l) => l.startsWith("View "))).toBe(false);
  });

  it("reaches the pull request from a terminal review row (AC-2)", async () => {
    // The same plumbing question for the other kind: the number is on the row,
    // the URL comes from the repo's remote, and both have to arrive.
    state.reviews = [review({ state: "completed", review_pr_number: 341 })];
    state.workspace = { git_remote_url: "git@github.com:nook-os/nook-os.git" };
    const opened = vi.spyOn(window, "open").mockImplementation(() => null);
    renderRuns();
    await waitFor(() => expect(screen.getAllByTestId("run-row")).toHaveLength(1));
    openBy.rightClick(screen.getAllByTestId("run-row")[0]);
    await waitFor(() => expect(menuLabels()).toContain("View PR #341"));

    await act(async () => {
      fireEvent.click(menuItem("View PR #341"));
    });
    expect(opened).toHaveBeenCalledWith(
      "https://github.com/nook-os/nook-os/pull/341",
      "_blank",
      "noreferrer",
    );
    opened.mockRestore();
  });

  it("offers no PR link when the repo's remote is not one it can address", async () => {
    state.reviews = [review({ state: "completed" })];
    state.workspace = { git_remote_url: "/workspace/nook-dogfood.git" };
    renderRuns();
    openBy.rightClick((await screen.findAllByTestId("run-row"))[0]);
    expect(menuLabels().some((l) => l.startsWith("View "))).toBe(false);
  });

  it("re-runs a failed run through the endpoint that takes it (AC-2)", async () => {
    state.builds = [build({ state: "failed" })];
    renderRuns();
    openBy.rightClick((await screen.findAllByTestId("run-row"))[0]);
    await act(async () => {
      fireEvent.click(menuItem("Re-run"));
    });
    expect(state.posts).toEqual([{ path: "/api/v1/jobs/{id}/rerun", id: "job-b1" }]);
    // No confirmation: re-run creates work, it does not destroy any.
    expect(screen.queryByText(/Cancel build run/)).toBeNull();
  });
});

describe("the selected run's header (MAIN-559 AC-7)", () => {
  it("states what the run is, where it is, and when it started", async () => {
    state.builds = [build({ state: "running" })];
    renderRuns();
    await screen.findAllByTestId("run-row");
    const header = screen.getByTestId("run-header");
    expect(within(header).getByRole("img", { name: "kind: build" })).toBeTruthy();
    expect(within(header).getByRole("img", { name: "state: running" })).toBeTruthy();
    expect(header.textContent).toContain("MAIN-42");
    expect(within(header).getByTestId("run-header-started")).toBeTruthy();
    // The exact instant behind the relative word, as on a row.
    expect(within(header).getByTestId("run-header-started").getAttribute("title")).toBe(
      "2026-08-08T09:00:00Z",
    );
    expect(within(header).getByTestId("run-header-elapsed").textContent).not.toBe("");
  });

  it("shows a review's head where a build shows its branch", async () => {
    state.reviews = [review({ state: "running" })];
    renderRuns();
    await screen.findAllByTestId("run-row");
    expect(screen.getByTestId("run-header-ref").textContent).toContain("abcdef1");
  });

  it("promotes ONE action and puts the rest behind an overflow", async () => {
    state.builds = [build({ state: "running" })];
    renderRuns();
    await screen.findAllByTestId("run-row");
    const primary = screen.getByTestId("run-primary-action");
    expect(primary.textContent).toBe("Cancel run");
    expect(primary.hasAttribute("disabled")).toBe(false);

    fireEvent.click(screen.getByTestId("run-header-overflow"));
    // The rest — and NOT the button already on screen beside it.
    const labels = screen
      .queryAllByRole("menuitem")
      .map((i) => i.querySelector(".ctxmenu-label")?.textContent?.trim() ?? "");
    expect(labels).toEqual(["Open", "Copy run ID", "Copy link"]);
  });

  it("disables the primary action with its reason where the API refuses it (AC-6)", async () => {
    state.builds = [build({ state: "completed" })];
    renderRuns();
    await screen.findAllByTestId("run-row");
    const primary = screen.getByTestId("run-primary-action");
    expect(primary.textContent).toBe("Re-run");
    expect(primary.hasAttribute("disabled")).toBe(true);
    expect(primary.getAttribute("title")).toBe("only a failed or canceled run can be re-run");
  });

  it("does not repeat the branch the outcome strip is already showing", async () => {
    // One fact, one place. `BuildOutcome` keeps the ticket and the PR; the
    // header above it keeps the branch.
    state.builds = [build({ state: "running" })];
    renderRuns();
    await screen.findAllByTestId("run-row");
    expect(screen.queryAllByTestId("build-branch")).toHaveLength(0);
  });
});

describe("the words and links an action needs", () => {
  it("builds a review's PR link the way the control plane builds it", () => {
    for (const remote of [
      "git@github.com:nook-os/nook-os.git",
      "https://github.com/nook-os/nook-os.git",
      "https://github.com/nook-os/nook-os",
      "ssh://git@github.com/nook-os/nook-os.git",
    ]) {
      expect(prWebUrl(remote, 341)).toBe("https://github.com/nook-os/nook-os/pull/341");
    }
    // Anything this cannot read an owner/repo out of gets NO link — the action
    // is then absent rather than pointing at a 404.
    expect(prWebUrl("git@gitlab.com:acme/app.git", 1)).toBeNull();
    expect(prWebUrl("/workspace/nook-dogfood.git", 1)).toBeNull();
    expect(prWebUrl(null, 1)).toBeNull();
    expect(prWebUrl("git@github.com:a/b.git", null)).toBeNull();
  });

  it("copies a link that selects the run it came from", () => {
    // A "Copy link" whose link did not open this run would be a lie; the run
    // is in the URL precisely so it is not one.
    const href = runHref("/workspaces/ws-1", "?kind=build", "job-b1");
    const p = new URLSearchParams(href.slice(href.indexOf("?")));
    expect(href.startsWith("/workspaces/ws-1?")).toBe(true);
    expect(p.get("run")).toBe("job-b1");
    expect(p.get("section")).toBe("runs");
    expect(p.get("kind")).toBe("build");
  });

  it("says canceling only while this client's own cancel is out", () => {
    expect(shownState("running", "cancel")).toBe("canceling");
    // Not on a run that has already ended — the transition is over.
    expect(shownState("completed", "cancel")).toBe("completed");
    expect(shownState("running", "rerun")).toBe("running");
    expect(shownState("running", undefined)).toBe("running");
    // A word the loop never stores, so it is not in `jobStateMeta`'s set.
    expect(runStateMeta("canceling")).toEqual({ label: "canceling", tone: "warn" });
    expect(runStateMeta("running").label).toBe("running");
  });
});
