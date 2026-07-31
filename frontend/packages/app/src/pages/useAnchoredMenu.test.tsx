// The clamp that keeps a portalled menu on screen (MAIN-300).
//
// `useAnchoredMenu` can only measure the TRIGGER, which is the right width for a
// select — the menu matches it — and badly wrong for a menu hung off a 22px icon
// button: clamped by 22px, a 132px menu still ran off the right edge, which is
// exactly what ChatView's three-dots menu did. `width` lets a caller say how wide
// the popup really is; omitting it must reproduce the old placement exactly, or
// every existing menu in the app moves.
//
// jsdom lays nothing out — every rect is zero — so the trigger's rect is stubbed.
// That is the whole input to the calculation under test, so stubbing it tests the
// arithmetic and nothing else.
import React, { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useAnchoredMenu } from "@nookos/ui";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

const VIEWPORT = 400;
/** A narrow trigger hard against the right edge — where the clamp has to bite. */
const TRIGGER = { left: 380, right: 400, width: 20, top: 100, bottom: 120, height: 20 };

function Harness({ width }: { width?: number }) {
  const [open, setOpen] = useState(false);
  const { hostRef, portal } = useAnchoredMenu(open, () => setOpen(false), {
    height: 80,
    width,
  });
  return (
    <div>
      <div ref={hostRef}>
        <button onClick={() => setOpen(true)}>open</button>
      </div>
      {portal(<span>item</span>, "probe-menu")}
    </div>
  );
}

async function openAndMeasure(width?: number): Promise<number> {
  Object.defineProperty(window, "innerWidth", { value: VIEWPORT, configurable: true });
  vi.spyOn(HTMLDivElement.prototype, "getBoundingClientRect").mockReturnValue(
    TRIGGER as DOMRect,
  );
  render(<Harness width={width} />);
  await userEvent.click(screen.getByText("open"));
  const menu = document.querySelector(".probe-menu") as HTMLElement;
  return Number.parseFloat(menu.style.left);
}

describe("useAnchoredMenu right-edge clamp", () => {
  it("clamps by the trigger's width when no width is given (unchanged)", async () => {
    // min(380, 400 - 20 - 4) = 376 — the placement every existing caller has.
    expect(await openAndMeasure()).toBe(376);
  });

  it("clamps by the menu's own width when one is given", async () => {
    // min(380, 400 - 132 - 4) = 264, so all 132px fit with the same 4px margin.
    const left = await openAndMeasure(132);
    expect(left).toBe(264);
    expect(left + 132).toBeLessThanOrEqual(VIEWPORT);
  });
});
