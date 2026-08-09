// The repo's review surface (MAIN-455 AC-5).
//
// What is worth pinning is that a review is READ, not driven, and that two runs
// of the same PR are distinguishable. Before this, a review left a tmux session
// that vanished when it died, so "which PR, at which commit, and what did it
// say" had no answer at all.
import React from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

const state = vi.hoisted(() => ({
  runs: [] as unknown[],
  transcript: [] as unknown[],
}));

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string) => {
      if (path.includes("/reviews")) return { data: state.runs };
      if (path.includes("/jobs/")) return { data: { transcript: state.transcript } };
      return { data: null };
    }),
  },
}));

import { pillTone, runLabel, shortHead, WorkspaceReviews } from "./WorkspaceReviews";

const run = (over: Record<string, unknown> = {}) => ({
  id: "job-1",
  state: "completed",
  review_pr_number: 341,
  review_head_sha: "abcdef1234567890",
  created_at: "2026-08-08T10:00:00Z",
  ...over,
});

beforeEach(() => {
  cleanup();
  state.runs = [];
  state.transcript = [];
});

function renderReviews() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <WorkspaceReviews workspaceId="ws-1" />
    </QueryClientProvider>,
  );
}

describe("labels", () => {
  it("names the pull request a run owns", () => {
    expect(runLabel(run() as never)).toBe("PR #341");
  });

  it("does not invent a PR for a run that has none", () => {
    expect(runLabel(run({ review_pr_number: null }) as never)).toBe("review");
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

describe("WorkspaceReviews", () => {
  it("says what a repo with no runs is waiting for", async () => {
    renderReviews();
    expect(
      (await screen.findByText(/No review has run for this repo yet/i)).textContent,
    ).toMatch(/pushed to/);
  });

  it("lists a run with its PR and head", async () => {
    state.runs = [run()];
    renderReviews();
    const row = await screen.findByTestId("review-run");
    expect(row.textContent).toContain("PR #341");
    expect(row.textContent).toContain("abcdef1");
  });

  it("shows two runs of ONE pull request as two entries", async () => {
    // The wakeup rule made visible: same PR, different head, so a list of two
    // is two pushes rather than the loop spinning.
    state.runs = [run(), run({ id: "job-2", review_head_sha: "999888777" })];
    renderReviews();
    const rows = await screen.findAllByTestId("review-run");
    expect(rows).toHaveLength(2);
    expect(rows[0].textContent).not.toEqual(rows[1].textContent);
  });

  it("renders the transcript of the newest run without being asked", async () => {
    state.runs = [run()];
    state.transcript = [
      { id: "t1", source: "agent", content: "Reviewed PR #341", at: "2026-08-08T10:01:00Z" },
    ];
    renderReviews();
    // findByText, not findByTestId: the panel exists immediately holding its
    // empty state, so asserting on the container would race the second query.
    expect(await screen.findByText(/Reviewed PR #341/)).toBeTruthy();
  });

  it("has no composer at all — hidden, not disabled", async () => {
    // A spec run's composer is how a human shapes the draft. A review is the
    // control plane's work: a greyed box would promise a capability that is
    // switched off, so there must be NO box. `disabled` passing this test was
    // the bug — it looked broken under every finished review.
    state.runs = [run()];
    state.transcript = [
      { id: "t1", source: "agent", content: "hello", at: "2026-08-08T10:01:00Z" },
    ];
    renderReviews();
    await screen.findByTestId("review-transcript");
    expect(document.querySelector(".chat-composer")).toBeNull();
    expect(document.querySelector("textarea")).toBeNull();
  });

  it("renders the agent's markdown as markdown, not punctuation", async () => {
    // The e2e's own screenshot showed `**Pass ends with no action…**` as
    // literal asterisks: this mapping had drifted behind the Loop page's.
    state.runs = [run()];
    state.transcript = [
      {
        id: "t1",
        source: "agent",
        content: "## Verdict\n\n**No action** — already reviewed.",
        at: "2026-08-08T10:01:00Z",
      },
    ];
    renderReviews();
    const strong = await screen.findByText("No action");
    expect(strong.tagName).toBe("STRONG");
    expect(screen.getByText("Verdict").tagName).toBe("H2");
  });
});

describe("transcript export (MAIN-471)", () => {
  it("copies the FULL transcript even when the view folds agent activity", async () => {
    state.runs = [run()];
    state.transcript = [
      { id: "l1", job_id: "job-1", source: "system", content: "started", at: "2026-08-08T10:00:00Z" },
      // A tool-activity ladder line the VIEW folds away.
      { id: "l2", job_id: "job-1", source: "agent", content: "· Bash cargo test", at: "2026-08-08T10:00:01Z" },
      { id: "l3", job_id: "job-1", source: "system", content: "verdict: approved", at: "2026-08-08T10:00:02Z" },
    ];
    const writeText = vi.fn(async (_text: string) => {});
    Object.assign(navigator, { clipboard: { writeText } });

    renderReviews();
    fireEvent.click(await screen.findByTestId("transcript-copy"));
    await new Promise((r) => setTimeout(r, 0));
    const copied = writeText.mock.calls[0][0] as string;
    expect(copied).toContain("· Bash cargo test");
    expect(copied).toContain("## agent");
    expect(copied).toContain("verdict: approved");
  });
});
