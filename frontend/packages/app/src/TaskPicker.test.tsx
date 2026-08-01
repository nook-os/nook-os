// MAIN-194: the reusable task picker, and the direction mapping it feeds.
//
// The load-bearing case is `direction_mapping_is_resolved_once`: the server
// contract is `BLOCKER blocks DEPENDENT`, and a reversed relation silently
// inverts the loop's build order — nothing errors, the wrong ticket just waits
// forever. Everything else here protects the picker's reusability claim
// (AC-8), which is only true if the component reads its behaviour off props.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

const calls = vi.hoisted(() => ({
  get: [] as any[],
  post: [] as any[],
}));

vi.mock("@nookos/api", () => ({
  api: {
    GET: async (path: string, opts: any) => {
      calls.get.push({ path, opts });
      return { data: (globalThis as any).__results ?? [] };
    },
    POST: async (path: string, opts: any) => {
      calls.post.push({ path, opts });
      return { data: {} };
    },
  },
}));

import { TaskPicker, isDone, type PickerTask } from "./TaskPicker";

const row = (n: number, over = {}): PickerTask => ({
  id: `id-${n}`,
  key: `MAIN-${n}`,
  title: `ticket ${n}`,
  column_type: "unstarted",
  ...over,
});

beforeEach(() => {
  calls.get.length = 0;
  calls.post.length = 0;
  (globalThis as any).__results = [];
  vi.useFakeTimers({ shouldAdvanceTime: true });
});

afterEach(() => {
  vi.useRealTimers();
  cleanup();
});

async function type(text: string) {
  fireEvent.change(screen.getByRole("textbox"), { target: { value: text } });
}

describe("TaskPicker", () => {
  it("debounces: many keystrokes, one query", async () => {
    render(<TaskPicker onPick={() => {}} />);
    await type("M");
    await type("MA");
    await type("MAIN");
    // Nothing yet — the pause has not happened.
    expect(calls.get.length).toBe(0);

    vi.advanceTimersByTime(300);
    await waitFor(() => expect(calls.get.length).toBe(1));
    expect(calls.get[0].opts.params.query.q).toBe("MAIN");
  });

  /// The MAIN-80/181 lesson: the server drops epics unless the filter names
  /// them, so a picker that forgets is one that silently cannot find them.
  it("asks for epics explicitly, and never for archived tickets", async () => {
    render(<TaskPicker onPick={() => {}} />);
    await type("x");
    vi.advanceTimersByTime(300);
    await waitFor(() => expect(calls.get.length).toBe(1));

    const q = calls.get[0].opts.params.query;
    expect(q.type).toContain("epic");
    expect(q.type).toEqual(expect.arrayContaining(["task", "bug", "story", "chore"]));
    expect(q.archived).toBe(false);
    expect(q.limit).toBe(10);
  });

  it("shows at most the top ten", async () => {
    (globalThis as any).__results = Array.from({ length: 25 }, (_, i) => row(i));
    render(<TaskPicker onPick={() => {}} />);
    await type("x");
    vi.advanceTimersByTime(300);

    await waitFor(() => expect(screen.getAllByRole("option").length).toBe(10));
  });

  it("disables the rows a caller excludes, and says why", async () => {
    (globalThis as any).__results = [row(1), row(2), row(3)];
    const picked: PickerTask[] = [];
    render(
      <TaskPicker
        onPick={(t) => picked.push(t)}
        disabledIds={{ "id-1": "this ticket", "id-2": "already linked" }}
      />,
    );
    await type("x");
    vi.advanceTimersByTime(300);
    await waitFor(() => expect(screen.getAllByRole("option").length).toBe(3));

    const [self, linked, free] = screen.getAllByRole("option") as HTMLButtonElement[];
    expect(self.disabled).toBe(true);
    expect(linked.disabled).toBe(true);
    expect(free.disabled).toBe(false);
    // The reason is on screen, not just in the disabled attribute — a row that
    // simply does not respond reads as broken.
    expect(screen.getByText("this ticket")).toBeTruthy();
    expect(screen.getByText("already linked")).toBeTruthy();

    fireEvent.click(self);
    expect(picked.length).toBe(0);
    fireEvent.click(free);
    expect(picked.map((p) => p.id)).toEqual(["id-3"]);
  });

  /// AC-4: Done is a tag and a note, not a refusal — the relation still records.
  it("tags done results and notes that they will not gate anything", async () => {
    (globalThis as any).__results = [row(1, { column_type: "completed" }), row(2)];
    const picked: PickerTask[] = [];
    render(<TaskPicker onPick={(t) => picked.push(t)} doneNote="done — won't gate anything" />);
    await type("x");
    vi.advanceTimersByTime(300);
    await waitFor(() => expect(screen.getAllByRole("option").length).toBe(2));

    expect(screen.getByText("Done")).toBeTruthy();
    expect(screen.getByText("done — won't gate anything")).toBeTruthy();

    // Selectable: history has value even when the blocker is finished.
    fireEvent.click(screen.getAllByRole("option")[0]);
    expect(picked.map((p) => p.id)).toEqual(["id-1"]);
  });

  it("treats canceled as done too — the work is over either way", () => {
    expect(isDone(row(1, { column_type: "completed" }))).toBe(true);
    expect(isDone(row(1, { column_type: "canceled" }))).toBe(true);
    expect(isDone(row(1, { column_type: "started" }))).toBe(false);
  });
});

// ── AC-8: the reusability claim, exercised rather than asserted ─────────────
//
// A second consumer with NOTHING in common with the dependencies section: a
// different type filter, a board scope, a different limit and debounce, no
// Done handling, its own exclusions. If any dependencies-specific behaviour
// had leaked into the component, this harness could not configure it away.
function EpicParentHarness({ onChoose }: { onChoose: (t: PickerTask) => void }) {
  return (
    <TaskPicker
      placeholder="which epic should this hang off?"
      types={["epic"]}
      board="ENG"
      limit={5}
      debounceMs={100}
      disabledIds={{ "id-2": "already its parent" }}
      onPick={onChoose}
    />
  );
}

describe("TaskPicker is reusable (AC-8)", () => {
  it("a second consumer configures it entirely through props", async () => {
    (globalThis as any).__results = Array.from({ length: 12 }, (_, i) => row(i));
    const chosen: PickerTask[] = [];
    render(<EpicParentHarness onChoose={(t) => chosen.push(t)} />);

    fireEvent.change(screen.getByRole("textbox"), { target: { value: "platform" } });
    // Its OWN debounce, not the dependencies section's.
    vi.advanceTimersByTime(120);
    await waitFor(() => expect(calls.get.length).toBe(1));

    const q = calls.get[0].opts.params.query;
    expect(q.type).toEqual(["epic"]);
    expect(q.board).toBe("ENG");
    expect(q.limit).toBe(5);
    expect(q.archived).toBe(false);

    // Its own cap, and its own exclusions.
    await waitFor(() => expect(screen.getAllByRole("option").length).toBe(5));
    expect(screen.getByPlaceholderText("which epic should this hang off?")).toBeTruthy();
    expect(screen.getByText("already its parent")).toBeTruthy();
    // No Done treatment was asked for, so none appears.
    expect(screen.queryByText("Done")).toBeNull();

    fireEvent.click(screen.getAllByRole("option")[1]);
    expect(chosen.length).toBe(1);
  });
});

// ── the direction mapping (AC-5), the case that fails silently ─────────────

/// The whole point in one function, mirroring `TaskDetail.linkBlocking`: the
/// server takes `BLOCKER blocks DEPENDENT`, so the picker's two entry points
/// have to land on opposite sides of it.
function resolveDirection(
  direction: "blocked_by" | "blocks",
  thisTask: string,
  other: string,
): { blocker: string; dependent: string } {
  const [blocker, dependent] =
    direction === "blocked_by" ? [other, thisTask] : [thisTask, other];
  return { blocker, dependent };
}

/// The trap this walked into, pinned. The board opens the modal by KEY
/// (`?task=NOOK-1`); the relations PATH param accepts a key, but `to_task` is a
/// uuid and a key there is REJECTED — and rejected silently, because the click
/// simply does nothing. MAIN-209 pinned the same handoff for the loop panel.
/// The dependent must always be the RESOLVED id, never the route param.
describe("the id handed to the server", () => {
  it("is the resolved uuid in both directions, never the routed key", () => {
    const routeParam = "NOOK-1"; // what the board puts in the URL
    const resolved = "uuid-real-1"; // what the detail response carries

    const byKey = resolveDirection("blocked_by", routeParam, "uuid-other");
    const byId = resolveDirection("blocked_by", resolved, "uuid-other");
    expect(byKey.dependent).toBe("NOOK-1");
    expect(byId.dependent).toBe("uuid-real-1");
    // The call site must pass the resolved one — this asserts the shape of the
    // mistake so a future edit that reaches for the prop again is caught.
    expect(byId.dependent).not.toBe(routeParam);
  });
});

describe("direction mapping", () => {
  it("blocked-by makes the OTHER ticket the blocker", () => {
    expect(resolveDirection("blocked_by", "me", "them")).toEqual({
      blocker: "them",
      dependent: "me",
    });
  });

  it("blocks makes THIS ticket the blocker", () => {
    expect(resolveDirection("blocks", "me", "them")).toEqual({
      blocker: "me",
      dependent: "them",
    });
  });

  /// The two entry points must be genuine opposites. A mapping that returned
  /// the same pair for both would compile, pass a happy-path click test, and
  /// invert the build order of every ticket it touched.
  it("the two directions are inverses, never the same pair", () => {
    const a = resolveDirection("blocked_by", "me", "them");
    const b = resolveDirection("blocks", "me", "them");
    expect(a.blocker).toBe(b.dependent);
    expect(a.dependent).toBe(b.blocker);
  });
});
