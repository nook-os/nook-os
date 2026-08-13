// MAIN-182: render-level proof that collapsing an epic actually removes its
// children from the DOM (not merely that the `nextCollapsed` helper is correct —
// that unit test passed while the bug shipped). Collapse must persist across a
// remount (reload) too. jsdom only.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { TaskItem, BoardColumn } from "@nookos/api";
import type { BacklogGroups, EpicSection } from "./Board";
import { BACKLOG_PREVIEW_MAX, BoardBacklog } from "./BoardBacklog";
import { useBacklogSelection } from "./backlogSelection";

const task = (id: string, title: string, type = "task"): TaskItem =>
  ({ id, key: id.toUpperCase(), title, type, column_id: "col-backlog" }) as unknown as TaskItem;

const section = (epicId: string, children: TaskItem[]): EpicSection => ({
  epic: task(epicId, `Epic ${epicId}`, "epic"),
  children,
  done: 0,
  total: children.length,
});

const childAlpha = task("a", "child alpha");
const childBeta = task("b", "child beta");
const groups: BacklogGroups = {
  epics: [section("e1", [childAlpha, childBeta])],
  noEpic: [],
};

const colTypeById = new Map<string, string | undefined>([["col-backlog", "backlog"]]);
const noop = () => {};

function renderBacklog(searching = false) {
  return render(
    <BoardBacklog
      groups={groups}
      colTypeById={colTypeById}
      wsName={new Map()}
      activeId={null}
      searching={searching}
      selected={new Set()}
      blockedIds={new Set()}
      canSendToBoard
      onAddEpic={noop}
      onAddChild={noop}
      onAddBacklog={noop}
      onOpen={noop}
      onMenu={noop}
      onToggleSelect={noop}
      onSendToBoard={noop}
      onDispatch={noop}
      columns={[] as BoardColumn[]}
      members={[]}
      onBulk={vi.fn(async () => null)}
    />,
  );
}

beforeEach(() => {
  localStorage.clear();
  useBacklogSelection.getState().clear();
});
afterEach(cleanup);

describe("BoardBacklog epic collapse (MAIN-182)", () => {
  it("the collapse chevron is a visible always-on control, not the hover-hidden card-menu-btn", () => {
    // The MAIN-182 root cause: the chevron reused `.card-menu-btn` (opacity:0,
    // revealed only by `.board-card:hover`, which the epic head never matches),
    // so it was invisible and clicks hit the head (opening the epic). Guard that
    // the control is its own always-visible class.
    renderBacklog();
    const chevron = screen.getByTitle("collapse");
    expect(chevron.className).toContain("backlog-epic-collapse");
    expect(chevron.className).not.toContain("card-menu-btn");
  });

  it("collapsing an epic removes its children from the DOM; expanding restores them", async () => {
    renderBacklog();
    expect(screen.getByText("child alpha")).toBeTruthy();
    expect(screen.getByText("child beta")).toBeTruthy();

    // The epic head's chevron toggles collapse (title flips collapse/expand).
    await userEvent.click(screen.getByTitle("collapse"));

    expect(screen.queryByText("child alpha")).toBeNull();
    expect(screen.queryByText("child beta")).toBeNull();

    await userEvent.click(screen.getByTitle("expand"));
    expect(screen.getByText("child alpha")).toBeTruthy();
  });

  it("collapse persists across a remount (reload)", async () => {
    const { unmount } = renderBacklog();
    await userEvent.click(screen.getByTitle("collapse"));
    expect(screen.queryByText("child alpha")).toBeNull();
    unmount();

    // A fresh mount reads the persisted collapse from localStorage.
    renderBacklog();
    expect(screen.queryByText("child alpha")).toBeNull();
    expect(screen.getByTitle("expand")).toBeTruthy();
  });

  it("a search override shows children even under a stored-collapsed epic, without erasing the stored state", async () => {
    // Pre-collapse e1, then render in searching mode: children are visible.
    renderBacklog();
    await userEvent.click(screen.getByTitle("collapse"));
    cleanup();

    renderBacklog(true);
    expect(screen.getByText("child alpha")).toBeTruthy();
    cleanup();

    // Clearing search restores the stored collapse untouched.
    renderBacklog(false);
    expect(screen.queryByText("child alpha")).toBeNull();
  });
});

// MAIN-571: the row's cells. jsdom applies no CSS, so what is provable here is
// the half the stylesheet cannot fix on its own — that the OPTIONAL elements
// still emit their cell when they have nothing in them. A collapsed cell is
// what made a priority-0 plain `task` start its key a badge-and-a-gap left of
// the `bug` above it, and no track definition can recover a cell that is not
// in the DOM. The widths themselves are asserted in `backlogRowStyles.test.ts`.
describe("BoardBacklog row alignment (MAIN-571)", () => {
  const withMeta = (t: TaskItem, extra: Partial<TaskItem>): TaskItem =>
    ({ ...t, ...extra }) as TaskItem;

  /** The row's cells, in order — its "track definition" as the DOM has it. */
  const cellsOf = (row: Element): string[] =>
    Array.from(row.children).map((c) => {
      const cls = Array.from(c.classList).find((n) => n.startsWith("backlog-row-"));
      return cls ?? c.className;
    });

  function renderMixed() {
    // Deliberately the three shapes AC-2 names, plus a fully-loaded row.
    const plain = withMeta(task("plain", "a plain task"), { priority: 0 });
    const bug = withMeta(task("bug", "a bug", "bug"), {
      priority: 1,
      labels: [
        { id: "l1", name: "ui", color: "#f00", tenant_id: "t1", created_at: "2026-08-13T00:00:00Z" },
      ],
      workspace_id: "ws1",
      description: "some words about the bug",
    });
    const loose = withMeta(task("loose", "a top-level task"), { priority: 2 });
    return render(
      <BoardBacklog
        groups={{ epics: [section("e2", [plain, bug])], noEpic: [loose] }}
        colTypeById={colTypeById}
        wsName={new Map([["ws1", "nookos"]])}
        activeId={null}
        searching={false}
        selected={new Set()}
        blockedIds={new Set()}
        canSendToBoard
        onAddEpic={noop}
        onAddChild={noop}
        onAddBacklog={noop}
        onOpen={noop}
        onMenu={noop}
        onToggleSelect={noop}
        onSendToBoard={noop}
        onDispatch={noop}
        columns={[] as BoardColumn[]}
        members={[]}
        onBulk={vi.fn(async () => null)}
      />,
    );
  }

  it("gives every row the same cells in the same order, whatever it is missing (AC-1/2)", () => {
    const { container } = renderMixed();
    const rows = Array.from(container.querySelectorAll(".backlog-row"));
    expect(rows.length).toBe(3);
    const expected = [
      "backlog-row-check",
      "backlog-row-prio",
      "backlog-row-type",
      "backlog-row-key",
      "backlog-row-title",
      "backlog-row-status",
      "backlog-row-meta",
      "backlog-row-preview",
      "backlog-row-actions",
    ];
    for (const row of rows) expect(cellsOf(row)).toEqual(expected);
  });

  it("keeps the priority and type cells for a priority-0 plain task (AC-2)", () => {
    const { container } = renderMixed();
    const plain = container.querySelector(".backlog-row")!;
    // Present, and empty: reserved space, not a glyph nobody asked for.
    expect(plain.querySelector(".backlog-row-prio")!.textContent).toBe("");
    expect(plain.querySelector(".backlog-row-type")!.children.length).toBe(0);
    expect(plain.querySelector(".card-prio")).toBeNull();
  });

  it("keeps the status cell on a row that has no status chip (AC-2/3)", () => {
    const { container } = renderMixed();
    const rows = Array.from(container.querySelectorAll(".backlog-row"));
    const child = rows[0];
    const topLevel = rows[2];
    // An epic child carries the chip; the "No epic" row never does.
    expect(child.querySelector(".backlog-status")).toBeTruthy();
    expect(topLevel.querySelector(".backlog-status")).toBeNull();
    // Both still have the cell it lives in.
    expect(topLevel.querySelector(".backlog-row-status")).toBeTruthy();
  });

  it("reserves the chevron's width on the chevron-less \"No epic\" head (AC-3)", () => {
    const { container } = renderMixed();
    const head = container.querySelector(".backlog-epic-head.no-epic")!;
    expect(head.querySelector(".backlog-epic-chevron-spacer")).toBeTruthy();
    // Still no collapse control — the section is not an epic.
    expect(head.querySelector(".backlog-epic-collapse")).toBeNull();
  });

  it("puts only a capped preview in the DOM, never the whole spec (AC-4)", () => {
    const long = `## Problem\n${"a word ".repeat(600)}`;
    const wordy = withMeta(task("wordy", "a card with a long spec"), { description: long });
    const { container } = render(
      <BoardBacklog
        groups={{ epics: [], noEpic: [wordy] }}
        colTypeById={colTypeById}
        wsName={new Map()}
        activeId={null}
        searching={false}
        selected={new Set()}
        blockedIds={new Set()}
        canSendToBoard
        onAddEpic={noop}
        onAddChild={noop}
        onAddBacklog={noop}
        onOpen={noop}
        onMenu={noop}
        onToggleSelect={noop}
        onSendToBoard={noop}
        onDispatch={noop}
        columns={[] as BoardColumn[]}
        members={[]}
        onBulk={vi.fn(async () => null)}
      />,
    );
    const preview = container.querySelector(".backlog-row-preview")!;
    expect(preview.textContent!.length).toBeLessThanOrEqual(BACKLOG_PREVIEW_MAX + 1);
    expect(preview.textContent!.endsWith("…")).toBe(true);
    expect(container.innerHTML.includes("a word ".repeat(40))).toBe(false);
  });
});
