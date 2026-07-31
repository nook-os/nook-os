// MAIN-174: one Type control, two modes.
//
// The board's Type filter used to be a row of toggle chips and the task modal
// had a dropdown; they are now the same component. The two things worth pinning
// are the two things that could regress:
//
//   - the FILTER keeps multi-type OR semantics (AC-2, NG-1) — picking two types
//     yields both, picking a chosen one clears it, and the menu stays open so
//     "bug or chore" is one gesture;
//   - the MODAL is unchanged (NG-3) — one pick, a plain string, menu closed.
//
// Rendered against the real component; `useAnchoredMenu` portals the menu to
// `document.body`, which `screen` queries reach.
import React from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { TypeSelect } from "@nookos/ui";

afterEach(() => cleanup());

/** Open the menu — the trigger is the only button before it exists. */
async function openMenu() {
  await userEvent.click(screen.getByRole("button", { name: /work type/i }));
}

/** Queries scoped to the open menu. The multiple-mode trigger shows the chosen
 *  type's name too, so an unscoped `getByText("Bug")` matches both it and the
 *  menu row — the assertion has to say which one it means. */
function menu() {
  const el = document.querySelector(".type-menu");
  if (!el) throw new Error("the type menu is not open");
  return within(el as HTMLElement);
}

describe("TypeSelect in the board filter (multiple)", () => {
  it("adds a type, then a second — the array is the OR set", async () => {
    const onChange = vi.fn();
    const { rerender } = render(
      <TypeSelect multiple value={[]} onChange={onChange} ariaLabel="filter by work type" />,
    );
    await openMenu();
    await userEvent.click(menu().getByText("Bug"));
    expect(onChange).toHaveBeenLastCalledWith(["bug"]);

    // The caller owns the state, so re-render with what it would have stored.
    rerender(
      <TypeSelect
        multiple
        value={["bug"]}
        onChange={onChange}
        ariaLabel="filter by work type"
      />,
    );
    // The menu is still open — that is the point of multiple mode.
    await userEvent.click(menu().getByText("Chore"));
    expect(onChange).toHaveBeenLastCalledWith(["bug", "chore"]);
  });

  it("clicking a chosen type clears it, exactly as the chip did", async () => {
    const onChange = vi.fn();
    render(
      <TypeSelect
        multiple
        value={["bug", "chore"]}
        onChange={onChange}
        ariaLabel="filter by work type"
      />,
    );
    await openMenu();
    await userEvent.click(menu().getByText("Bug"));
    expect(onChange).toHaveBeenLastCalledWith(["chore"]);
  });

  it("says what is selected without opening the menu", async () => {
    const { rerender } = render(
      <TypeSelect multiple value={[]} onChange={vi.fn()} ariaLabel="filter by work type" />,
    );
    expect(screen.getByTitle("any type")).toBeTruthy();

    rerender(
      <TypeSelect multiple value={["bug"]} onChange={vi.fn()} ariaLabel="filter by work type" />,
    );
    expect(screen.getByTitle("Bug")).toBeTruthy();

    rerender(
      <TypeSelect
        multiple
        value={["bug", "chore"]}
        onChange={vi.fn()}
        ariaLabel="filter by work type"
      />,
    );
    expect(screen.getByTitle("2 types")).toBeTruthy();
  });

  it("marks each chosen type as pressed — a set of toggles, not a menu of one", async () => {
    render(
      <TypeSelect
        multiple
        value={["bug"]}
        onChange={vi.fn()}
        ariaLabel="filter by work type"
      />,
    );
    await openMenu();
    const bug = menu().getByText("Bug").closest("button")!;
    const chore = menu().getByText("Chore").closest("button")!;
    expect(bug.getAttribute("aria-pressed")).toBe("true");
    expect(chore.getAttribute("aria-pressed")).toBe("false");
  });
});

describe("TypeSelect in the task modal (single) — unchanged", () => {
  it("picks one type as a plain string and closes", async () => {
    const onChange = vi.fn();
    render(<TypeSelect value="task" onChange={onChange} />);
    await openMenu();
    await userEvent.click(menu().getByText("Bug"));

    // A string, not an array — the modal's contract (NG-3).
    expect(onChange).toHaveBeenCalledWith("bug");
    expect(screen.queryByText("Change work type")).toBeNull();
  });

  it("re-picking the current type is a no-op, and still closes", async () => {
    const onChange = vi.fn();
    render(<TypeSelect value="bug" onChange={onChange} />);
    await openMenu();
    await userEvent.click(menu().getByText("Bug"));
    expect(onChange).not.toHaveBeenCalled();
    expect(screen.queryByText("Change work type")).toBeNull();
  });

  it("single mode carries no aria-pressed — it is a menu, not toggles", async () => {
    render(<TypeSelect value="task" onChange={vi.fn()} />);
    await openMenu();
    expect(menu().getByText("Bug").closest("button")!.hasAttribute("aria-pressed")).toBe(
      false,
    );
  });
});
