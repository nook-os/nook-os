// MAIN-603: what the sidebar does with content Nook does not understand.
//
// Rendered against the real component and the real shared `Markdown`, because
// the two claims worth pinning are both about that renderer: a GFM table has to
// come out as a table (AC-6 — the reason reports reuse the comment renderer
// rather than a new one), and a `<script>` a producer wrote has to be sanitised
// away rather than reaching the page (AC-6, NG-7). A test against a stub
// renderer would assert nothing about either.
import React from "react";
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import type { TaskReport } from "@nookos/api";
import { TaskReports } from "./TaskReports";

afterEach(() => cleanup());

function report(over: Partial<TaskReport> = {}): TaskReport {
  return {
    id: "00000000-0000-0000-0000-000000000001",
    task_id: "00000000-0000-0000-0000-0000000000ff",
    key: "build",
    title: "Build",
    body_md: "hello",
    author_type: "user",
    author_id: null,
    author_name: "ci",
    created_at: "2026-08-15T10:00:00Z",
    updated_at: "2026-08-15T10:00:00Z",
    ...over,
  };
}

describe("TaskReports", () => {
  it("renders a GFM table as a table", () => {
    render(
      <TaskReports
        reports={[
          report({ body_md: "| file | cov |\n|---|---|\n| a.rs | 91% |" }),
        ]}
      />,
    );
    const table = document.querySelector("table");
    expect(table).not.toBeNull();
    expect(screen.getByText("a.rs").closest("td")).not.toBeNull();
    expect(screen.getByText("file").closest("th")).not.toBeNull();
  });

  it("sanitises a script a producer wrote instead of putting it on the page", () => {
    render(
      <TaskReports
        reports={[
          report({
            body_md: "before\n\n<script>window.pwned = true</script>\n\nafter",
          }),
        ]}
      />,
    );
    expect(document.querySelector("script")).toBeNull();
    // The surrounding prose is untouched — sanitising is not swallowing.
    expect(screen.getByText("before")).toBeTruthy();
    expect(screen.getByText("after")).toBeTruthy();
  });

  it("shows each report's title and its update time, so a stale one looks stale", () => {
    render(
      <TaskReports
        reports={[
          report({ id: "1", key: "coverage", title: "Coverage", author_name: "ci" }),
          report({ id: "2", key: "bench", title: "Benchmark", author_name: "nightly" }),
        ]}
      />,
    );
    expect(screen.getByText("Coverage")).toBeTruthy();
    expect(screen.getByText("Benchmark")).toBeTruthy();
    // The order is the server's — most recently updated first — so the DOM is
    // rendered in the order it was given, never re-sorted here.
    const titles = Array.from(document.querySelectorAll(".task-report-title")).map(
      (e) => e.textContent,
    );
    expect(titles).toEqual(["Coverage", "Benchmark"]);
    const stamp = new Date("2026-08-15T10:00:00Z").toLocaleString();
    expect(screen.getAllByText(new RegExp(escapeRe(stamp)))).toHaveLength(2);
  });

  it("renders nothing at all when a card has no reports", () => {
    const { container } = render(<TaskReports reports={[]} />);
    expect(container.innerHTML).toBe("");
  });
});

function escapeRe(s: string) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
