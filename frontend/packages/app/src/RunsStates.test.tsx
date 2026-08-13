// The five states the runs list can be in instead of rows (MAIN-560 AC-5).
//
// Their whole job is to be told apart, so what is pinned is that each says
// something the others do not — and, for the two that are easy to conflate, that
// the filtered one can never be read as "this repo has never run anything".
import React from "react";
import { describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { RunGone, RunsState, runsPhase, type RunFilterChip } from "./RunsStates";

const noFilters: RunFilterChip[] = [];
// The words the chip row shows (MAIN-558), which is what these states repeat.
const twoFilters: RunFilterChip[] = [
  { key: "kind", label: "Reviews" },
  { key: "state:running", label: "running" },
];

const phase = (over: Partial<Parameters<typeof runsPhase>[0]> = {}) =>
  runsPhase({ loading: false, error: false, shown: 0, search: "", filters: noFilters, ...over });

describe("which state the list is in", () => {
  it("shows rows the moment there are any, whatever else is true", () => {
    // The rule `dataListPhase` already uses: appending a page must not flash a
    // state over a list somebody is reading, and a background refetch that
    // fails must leave the rows it has.
    expect(phase({ shown: 3, loading: true, error: true, search: "x" })).toBe("rows");
  });

  it("puts a failed load ahead of a load still in flight", () => {
    expect(phase({ loading: true, error: true })).toBe("error");
    expect(phase({ loading: true })).toBe("loading");
  });

  it("tells an empty search apart from an empty filter apart from an empty repo", () => {
    expect(phase({ search: "MAIN-999" })).toBe("no-search");
    expect(phase({ filters: twoFilters })).toBe("no-filters");
    expect(phase()).toBe("empty");
  });

  it("reads whitespace as no search at all", () => {
    expect(phase({ search: "   " })).toBe("empty");
  });

  it("lets the search speak when both it and the filters are on", () => {
    expect(phase({ search: "MAIN-999", filters: twoFilters })).toBe("no-search");
  });
});

const renderState = (over: Partial<React.ComponentProps<typeof RunsState>> = {}) => {
  cleanup();
  const props = {
    phase: "empty" as const,
    search: "",
    filters: noFilters,
    error: null,
    onRetry: vi.fn(),
    onClearSearch: vi.fn(),
    onClearFilters: vi.fn(),
    ...over,
  };
  render(<RunsState {...props} />);
  return props;
};

describe("each state, on its own trigger (AC-5)", () => {
  it("says a repo has never run anything", () => {
    renderState({ phase: "empty" });
    expect(screen.getByTestId("runs-empty").textContent).toMatch(/No run has happened in this repo yet/);
  });

  it("says the search matched nothing, quoting the term", () => {
    renderState({ phase: "no-search", search: "MAIN-999" });
    const said = screen.getByTestId("runs-no-search");
    expect(said.textContent).toContain("MAIN-999");
    // No filters are on, so nothing offers to clear any.
    expect(screen.queryByTestId("runs-clear-filters")).toBeNull();
  });

  it("names the filters as well when a search ran into them too", () => {
    renderState({ phase: "no-search", search: "MAIN-999", filters: twoFilters });
    expect(screen.getByTestId("runs-no-search-filters").textContent).toContain("Reviews");
    expect(screen.getByTestId("runs-clear-filters")).toBeTruthy();
  });

  it("names the filters that emptied the list, and can never read as an empty repo", () => {
    renderState({ phase: "no-filters", filters: twoFilters });
    const said = screen.getByTestId("runs-no-filters");
    expect(said.textContent).toContain("these filters: Reviews, running");
    expect(said.textContent).toContain("This repo has runs");
    expect(said.textContent).not.toMatch(/No run has happened/);
  });

  it("says a load failed, in the server's own words, with a retry", () => {
    const props = renderState({ phase: "error", error: "the control plane is unreachable" });
    expect(screen.getByTestId("runs-load-failed").textContent).toContain(
      "the control plane is unreachable",
    );
    fireEvent.click(screen.getByTestId("runs-retry"));
    expect(props.onRetry).toHaveBeenCalledTimes(1);
  });

  it("says the run the URL named is gone, and offers a way out", () => {
    cleanup();
    const onShowNewest = vi.fn();
    render(<RunGone onShowNewest={onShowNewest} />);
    expect(screen.getByTestId("run-gone").textContent).toMatch(/gone, or you are not allowed/);
    fireEvent.click(screen.getByTestId("run-gone-newest"));
    expect(onShowNewest).toHaveBeenCalledTimes(1);
  });

  it("hands each clear back to the caller", () => {
    const props = renderState({ phase: "no-search", search: "x", filters: twoFilters });
    fireEvent.click(screen.getByTestId("runs-clear-search"));
    fireEvent.click(screen.getByTestId("runs-clear-filters"));
    expect(props.onClearSearch).toHaveBeenCalledTimes(1);
    expect(props.onClearFilters).toHaveBeenCalledTimes(1);
  });
});
