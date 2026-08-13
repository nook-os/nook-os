// Why a repo with the switch ON is doing nothing (MAIN-387 AC-2).
//
// Each of these causes has its fix somewhere different — another page, the
// ceiling, the board, a node label — so the panel getting the cause WRONG is
// worse than saying nothing: it sends somebody to fix the thing that was
// already fine. The precedence is therefore the point of these tests, not a
// detail of them.
import { describe, expect, it } from "vitest";
import {
  branchOf,
  buildLoopWhy,
  buildOutcomeWords,
  concludedNothing,
  FAILURE_BACKOFF_MS,
  isLiveRun,
  pinLabel,
  whyWords,
  type BuildLoopSettings,
  type BuildRunRow,
} from "./buildLoop";

const NOW = Date.parse("2026-08-13T12:00:00Z");

const settings = (over: Partial<BuildLoopSettings> = {}): BuildLoopSettings => ({
  enabled: true,
  concurrency: 1,
  node_id: null,
  node_name: null,
  enabled_by: null,
  ...over,
});

const run = (over: Partial<BuildRunRow> = {}): BuildRunRow => ({
  id: "r1",
  state: "running",
  task_key: "MAIN-42",
  created_at: "2026-08-13T11:59:00Z",
  ...over,
});

/** A stable clock, so the sentence under test is the sentence and not the
 *  host's locale. */
const at = (t: number) => new Date(t).toISOString().slice(11, 19);

describe("isLiveRun", () => {
  it("counts the four states that hold this repo's ceiling", () => {
    for (const s of ["queued", "claimed", "running", "waiting_on_human"]) {
      expect(isLiveRun(s)).toBe(true);
    }
    for (const s of ["completed", "failed", "canceled"]) {
      expect(isLiveRun(s)).toBe(false);
    }
  });
});

describe("buildLoopWhy", () => {
  it("claims nothing about a repo whose settings have not arrived", () => {
    expect(
      buildLoopWhy({ tenantLoops: true, settings: undefined, runs: [], now: NOW }).kind,
    ).toBe("loading");
  });

  it("puts the tenant switch above every other cause — nothing else can undo it", () => {
    // Switch on, a card running, a ceiling to spare: still nothing will run.
    const why = buildLoopWhy({
      tenantLoops: false,
      settings: settings({ concurrency: 4 }),
      runs: [run()],
      now: NOW,
    });
    expect(why.kind).toBe("tenant-off");
  });

  it("does not read a settings query still in flight as loops being off", () => {
    const why = buildLoopWhy({
      tenantLoops: undefined,
      settings: settings(),
      runs: [],
      now: NOW,
    });
    expect(why.kind).toBe("no-work");
  });

  it("keeps the repo's switch and its ceiling apart", () => {
    expect(
      buildLoopWhy({ tenantLoops: true, settings: settings({ enabled: false }), runs: [], now: NOW })
        .kind,
    ).toBe("switch-off");
    expect(
      buildLoopWhy({ tenantLoops: true, settings: settings({ concurrency: 0 }), runs: [], now: NOW })
        .kind,
    ).toBe("ceiling-zero");
  });

  it("prefers a queued run's own gate to the ceiling it also happens to fill", () => {
    const why = buildLoopWhy({
      tenantLoops: true,
      settings: settings({ concurrency: 1 }),
      runs: [run({ state: "queued", queued_reason: "no eligible executor: no node of yours is online" })],
      now: NOW,
    });
    expect(why).toMatchObject({
      kind: "queued",
      reason: "no eligible executor: no node of yours is online",
    });
  });

  it("reports the ceiling when the live runs fill it", () => {
    const why = buildLoopWhy({
      tenantLoops: true,
      settings: settings({ concurrency: 2 }),
      runs: [run({ id: "a" }), run({ id: "b", state: "claimed" }), run({ id: "c", state: "completed" })],
      now: NOW,
    });
    expect(why).toEqual({ kind: "at-concurrency", live: 2, concurrency: 2 });
  });

  it("names the hold a concluded-nothing run put its card in", () => {
    const why = buildLoopWhy({
      tenantLoops: true,
      settings: settings(),
      runs: [run({ state: "failed" })],
      newest: { state: "failed", updated_at: "2026-08-13T11:58:00Z" },
      now: NOW,
    });
    expect(why).toEqual({
      kind: "backoff",
      until: Date.parse("2026-08-13T11:58:00Z") + FAILURE_BACKOFF_MS,
      taskKey: "MAIN-42",
    });
  });

  it("stops claiming a hold once it has expired", () => {
    const why = buildLoopWhy({
      tenantLoops: true,
      settings: settings(),
      runs: [run({ state: "failed" })],
      newest: { state: "failed", updated_at: "2026-08-13T11:50:00Z" },
      now: NOW,
    });
    expect(why.kind).toBe("no-work");
  });

  it("does not hold on a run that concluded something", () => {
    const why = buildLoopWhy({
      tenantLoops: true,
      settings: settings(),
      runs: [run({ state: "completed" })],
      newest: {
        state: "completed",
        build_outcome: "pr_opened",
        updated_at: "2026-08-13T11:59:30Z",
      },
      now: NOW,
    });
    expect(why.kind).toBe("no-work");
  });
});

describe("concludedNothing", () => {
  it("is the wakeup rule's own test, all three arms", () => {
    expect(concludedNothing({ state: "failed" })).toBe(true);
    expect(concludedNothing({ state: "canceled" })).toBe(true);
    expect(concludedNothing({ state: "completed", build_outcome: null })).toBe(true);
    expect(concludedNothing({ state: "completed", build_outcome: "blocked" })).toBe(false);
    expect(concludedNothing({ state: "running" })).toBe(false);
  });
});

describe("whyWords", () => {
  it("says which card is held, and until when", () => {
    const words = whyWords(
      { kind: "backoff", until: NOW + 60_000, taskKey: "MAIN-42" },
      at,
    );
    expect(words).toContain("MAIN-42");
    expect(words).toContain(at(NOW + 60_000));
  });

  it("passes a queued run's sentence through rather than re-wording it", () => {
    const reason = "no eligible executor: no node wears the role/build label";
    expect(whyWords({ kind: "queued", reason, run: run() })).toBe(reason);
  });

  it("reads singular at a ceiling of one and names both numbers above it", () => {
    expect(whyWords({ kind: "at-concurrency", live: 1, concurrency: 1 })).toBe(
      "at concurrency — one build run at a time",
    );
    expect(whyWords({ kind: "at-concurrency", live: 3, concurrency: 3 })).toContain("3 of 3");
  });

  it("says an empty board is an absence of work, not a fault", () => {
    expect(whyWords({ kind: "no-work" })).toContain("agent-ready");
  });
});

describe("pinLabel", () => {
  it("calls no pin Auto, which is what it means", () => {
    expect(pinLabel(settings())).toBe("Auto");
  });

  it("names the pinned machine", () => {
    expect(pinLabel(settings({ node_id: "n1", node_name: "azul" }))).toBe("azul");
  });

  it("never reads a pin at a vanished node as Auto — a run there goes nowhere", () => {
    expect(pinLabel(settings({ node_id: "n1", node_name: null }))).not.toBe("Auto");
  });
});

describe("buildOutcomeWords", () => {
  it("says what each of the three conclusions is", () => {
    expect(buildOutcomeWords("pr_opened")).toBe("PR opened");
    expect(buildOutcomeWords("blocked")).toContain("handed back");
    expect(buildOutcomeWords("nothing_to_do")).toBe("nothing to do");
  });

  it("shows a conclusion it has never heard of rather than inventing a phrase", () => {
    expect(buildOutcomeWords("something_later")).toBe("something_later");
  });

  it("says nothing for a run that concluded nothing", () => {
    expect(buildOutcomeWords(null)).toBeNull();
    expect(buildOutcomeWords(undefined)).toBeNull();
  });
});

describe("branchOf", () => {
  const locations = [
    { path: "/srv/repo", git_branch: "main" },
    { path: "/srv/repo__main-42", git_branch: "main-42-build-loop-ui" },
  ] as never;

  it("reads the branch off the checkout the run recorded", () => {
    expect(branchOf({ worktree_path: "/srv/repo__main-42" }, locations)).toBe(
      "main-42-build-loop-ui",
    );
  });

  it("prefers the card's own column, which a human's start-work stamped", () => {
    expect(branchOf({ branch: "hand-made", worktree_path: "/srv/repo" }, locations)).toBe(
      "hand-made",
    );
  });

  it("says nothing rather than guessing when the checkout is not known here", () => {
    expect(branchOf({ worktree_path: "/elsewhere" }, locations)).toBeNull();
    expect(branchOf({ worktree_path: "/srv/repo__main-42" }, undefined)).toBeNull();
    expect(branchOf(null, locations)).toBeNull();
  });
});
