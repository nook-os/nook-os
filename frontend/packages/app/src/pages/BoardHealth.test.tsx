// The Health tab renders a REPORT, not a list of problems (MAIN-570 AC-7).
//
// The distinction is the whole point of the zero rows: a board with nothing
// wrong must read as healthy, and a page that hid its clean checks would be
// indistinguishable from one that failed to load. And nothing on the tab may
// change anything (NG-3) — the only thing a row does is hand off to the Backlog.
import React from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { BoardHealth } from "@nookos/api";

import { BoardHealthTab, HEALTH_LABEL } from "./BoardHealth";

afterEach(cleanup);

const REPORT: BoardHealth = {
  board_id: "b-1",
  checks: [
    {
      check: "archived_not_done",
      count: 2,
      tasks: [
        { id: "t-67", key: "MAIN-67" },
        { id: "t-70", key: "MAIN-70" },
      ],
    },
    { check: "done_agent_ready", count: 1, tasks: [{ id: "t-154", key: "MAIN-154" }] },
    { check: "epics_closeable", count: 0, tasks: [] },
    { check: "epics_empty", count: 0, tasks: [] },
  ],
};

describe("the Health tab", () => {
  it("renders every check, including the ones that found nothing", () => {
    render(<BoardHealthTab report={REPORT} onPick={() => {}} />);
    for (const check of REPORT.checks) {
      expect(screen.getByText(HEALTH_LABEL[check.check])).toBeTruthy();
    }
    // A clean check says so rather than disappearing.
    expect(screen.getAllByText("none")).toHaveLength(2);
    // The offending cards are named by key, not just counted.
    expect(screen.getByText("MAIN-67")).toBeTruthy();
    expect(screen.getByText("MAIN-70")).toBeTruthy();
  });

  it("hands a non-zero check off to the backlog, and offers nothing on a zero one", async () => {
    const picked = vi.fn();
    render(<BoardHealthTab report={REPORT} onPick={picked} />);

    await userEvent.click(screen.getByText(HEALTH_LABEL.done_agent_ready));
    expect(picked).toHaveBeenCalledWith("done_agent_ready");

    // A clean check is not a destination — there is nothing to show.
    await userEvent.click(screen.getByText(HEALTH_LABEL.epics_empty));
    expect(picked).toHaveBeenCalledTimes(1);
  });

  it("offers no control that changes anything (NG-3)", () => {
    const { container } = render(<BoardHealthTab report={REPORT} onPick={() => {}} />);
    // Every button on the tab is a check row; there is no archive, no unarchive,
    // no "fix all". Remediation is the backlog's existing bulk toolbar.
    const buttons = [...container.querySelectorAll("button")];
    expect(buttons).toHaveLength(REPORT.checks.length);
    expect(buttons.every((b) => b.className.includes("board-health-row"))).toBe(true);
  });

  it("says so while the report is still in flight, rather than reading as healthy", () => {
    render(<BoardHealthTab report={undefined} onPick={() => {}} />);
    expect(screen.queryByText("none")).toBeNull();
  });
});
