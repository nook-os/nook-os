// MAIN-233: the pure half of the Loop workspace — what the composer is for,
// what a transcript entry IS, and what a run filed. All view-agnostic, so the
// page's decisions are testable without a DOM.
import { describe, expect, it } from "vitest";
import type { LoopJob } from "@nookos/api";
import { composerMode, filedKeys, looksLikeDraft, stripAnsi } from "./loop";

const job = (state: string): LoopJob =>
  ({ id: "j1", state, kind: "spec" }) as unknown as LoopJob;

describe("composerMode (MAIN-233)", () => {
  it("offers the seed box only when there is no job to talk to", () => {
    expect(composerMode(null)).toBe("seed");
    expect(composerMode(undefined)).toBe("seed");
  });

  it("steers every live state, including a run paused on a human", () => {
    for (const s of ["queued", "claimed", "running", "waiting_on_human"]) {
      expect(composerMode(job(s))).toBe("steer");
    }
  });

  it("goes read-only on a terminal job — the server refuses messages there", () => {
    for (const s of ["completed", "failed", "canceled"]) {
      expect(composerMode(job(s))).toBe("readonly");
    }
  });
});

describe("stripAnsi", () => {
  it("removes colour, cursor moves and control bytes, keeping the text", () => {
    expect(stripAnsi("\u001b[32mok\u001b[0m")).toBe("ok");
    expect(stripAnsi("a\u001b[2Kb")).toBe("ab");
    expect(stripAnsi("keep\u0007me")).toBe("keepme");
    // An OSC title sequence, terminated either way.
    expect(stripAnsi("\u001b]0;title\u0007after")).toBe("after");
  });

  it("leaves ordinary text — including newlines and tabs — alone", () => {
    expect(stripAnsi("line one\nline two\tend")).toBe("line one\nline two\tend");
  });
});

describe("looksLikeDraft", () => {
  it("recognises the issue shape the skills print", () => {
    expect(
      looksLikeDraft("## Problem\n\nx\n\n## Acceptance Criteria\n\n- [ ] AC-1 — y"),
    ).toBe(true);
    // Problem + Non-goals is the shape too, even without the AC heading in view.
    expect(looksLikeDraft("## Problem\n\nx\n\n## Non-goals\n\n- NG-1 — y")).toBe(true);
  });

  it("sees through terminal colour on the heading", () => {
    expect(looksLikeDraft("\u001b[1m## Acceptance Criteria\u001b[0m\n- [ ] AC-1")).toBe(
      true,
    );
  });

  it("does not mistake narration or a passing mention for a draft", () => {
    expect(looksLikeDraft("reading the codebase…")).toBe(false);
    expect(looksLikeDraft("I will write the Acceptance Criteria next")).toBe(false);
    expect(looksLikeDraft("## Problem\n\njust the one heading")).toBe(false);
  });
});

describe("filedKeys", () => {
  const line = (content: string) => ({ content });

  it("collects this board's keys in first-mention order, deduped", () => {
    expect(
      filedKeys(
        [line("Filed MAIN-42."), line("and MAIN-43"), line("MAIN-42 again")],
        "MAIN-7",
      ),
    ).toEqual(["MAIN-42", "MAIN-43"]);
  });

  it("excludes the job's own target — a spec job always names it", () => {
    expect(filedKeys([line("speccing MAIN-7, filed MAIN-9")], "MAIN-7")).toEqual([
      "MAIN-9",
    ]);
  });

  // The defect this closes: a drafted spec is FULL of AC-N/NG-N, so an
  // unanchored "uppercase word dash digits" match turned every draft into a
  // header of links to tickets that do not exist.
  it("never offers a draft's AC-N / NG-N tags, or other dashed tokens", () => {
    const draft = line(
      "## Acceptance Criteria\n- [ ] AC-1 — x\n- [ ] AC-2 — y\n" +
        "## Non-goals\n- NG-1 — z\nencode as UTF-8, hash with SHA-256",
    );
    expect(filedKeys([draft], "MAIN-7")).toEqual([]);
    // …and a real key in the same draft still comes through.
    expect(
      filedKeys([draft, line("Filed MAIN-88 under the epic.")], "MAIN-7"),
    ).toEqual(["MAIN-88"]);
  });

  it("ignores another board's keys — this run cannot have filed there", () => {
    expect(filedKeys([line("see OTHER-3 and MAIN-4")], "MAIN-7")).toEqual([
      "MAIN-4",
    ]);
  });

  it("offers nothing without a usable self key — no prefix to trust", () => {
    expect(filedKeys([line("Filed MAIN-42.")], null)).toEqual([]);
    expect(filedKeys([line("Filed MAIN-42.")], "not-a-key")).toEqual([]);
  });

  it("ignores lowercase words and bare numbers", () => {
    expect(filedKeys([line("see main-1 and issue 42 and PR #7")], "MAIN-7")).toEqual(
      [],
    );
  });

  it("is empty for an absent transcript", () => {
    expect(filedKeys(undefined, "MAIN-7")).toEqual([]);
    expect(filedKeys(null, "MAIN-7")).toEqual([]);
  });
});
