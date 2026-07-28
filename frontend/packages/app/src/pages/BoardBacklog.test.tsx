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
import { BoardBacklog } from "./BoardBacklog";
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
