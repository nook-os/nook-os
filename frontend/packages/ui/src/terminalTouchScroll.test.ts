// A synthesized finger drag must produce the events the mouse wheel produces —
// that IS the acceptance criterion (MAIN-621 AC-2): tmux owns the history, the
// wheel is the only thing that reaches it, and `scrollback: 0` means anything
// that does not end as a wheel report scrolls nothing at all.
import { beforeEach, describe, expect, it } from "vitest";
import { attachTouchScroll } from "./terminalTouchScroll";

const ROW_HEIGHT = 20;
/** Three rows per notch, as the module documents. */
const STEP = ROW_HEIGHT * 3;

let host: HTMLDivElement;
let xterm: HTMLDivElement;
let wheels: WheelEvent[];
let detach: () => void;

beforeEach(() => {
  document.body.innerHTML = "";
  host = document.createElement("div");
  // xterm's element is a CHILD of the host, which is why the wheel has to be
  // dispatched at it rather than at the element the touch arrived on.
  xterm = document.createElement("div");
  host.appendChild(xterm);
  document.body.appendChild(host);

  wheels = [];
  xterm.addEventListener("wheel", (e) => wheels.push(e as WheelEvent));
  detach = attachTouchScroll(host, {
    wheelTarget: () => xterm,
    rowHeight: () => ROW_HEIGHT,
  });
  return () => detach();
});

function touch(type: string, points: { x: number; y: number }[]): TouchEvent {
  const touches = points.map((p, i) => ({
    identifier: i,
    clientX: p.x,
    clientY: p.y,
    target: xterm,
  }));
  const ev = new TouchEvent(type, {
    touches: touches as unknown as Touch[],
    changedTouches: touches as unknown as Touch[],
    bubbles: true,
    cancelable: true,
  });
  xterm.dispatchEvent(ev);
  return ev;
}

/** A drag from `y` upward/downward through `points`, ending where it stopped. */
function drag(from: { x: number; y: number }, ...to: { x: number; y: number }[]) {
  touch("touchstart", [from]);
  for (const p of to) touch("touchmove", [p]);
  return touch("touchend", [to[to.length - 1] ?? from]);
}

describe("terminal touch scroll", () => {
  it("a drag UP scrolls into history, the direction the wheel scrolls up", () => {
    drag({ x: 100, y: 400 }, { x: 100, y: 400 - STEP });

    expect(wheels).toHaveLength(1);
    expect(wheels[0].deltaY).toBeLessThan(0);
    // Lines, not pixels — the same units xterm reads off a real wheel notch.
    expect(wheels[0].deltaMode).toBe(WheelEvent.DOM_DELTA_LINE);
    // The report carries the finger's position, so tmux scrolls the pane the
    // gesture was actually over.
    expect(wheels[0].clientX).toBe(100);
  });

  it("a drag DOWN scrolls back toward the live prompt", () => {
    drag({ x: 100, y: 100 }, { x: 100, y: 100 + STEP });

    expect(wheels).toHaveLength(1);
    expect(wheels[0].deltaY).toBeGreaterThan(0);
  });

  it("emits one event per notch, not one event carrying the whole delta", () => {
    // xterm forwards ONE mouse report per wheel event and reads the delta only
    // for its sign, so a three-notch drag that emitted a single event would
    // scroll tmux one notch.
    drag({ x: 10, y: 500 }, { x: 10, y: 500 - STEP * 3 });

    expect(wheels).toHaveLength(3);
    expect(wheels.every((w) => w.deltaY < 0)).toBe(true);
  });

  it("carries leftover travel between moves so a slow drag still scrolls", () => {
    // Rounding each frame to nothing is how a slow drag scrolls nothing at all;
    // the remainder has to survive to the next move.
    touch("touchstart", [{ x: 10, y: 500 }]);
    touch("touchmove", [{ x: 10, y: 500 - STEP * 0.6 }]);
    expect(wheels).toHaveLength(0);
    touch("touchmove", [{ x: 10, y: 500 - STEP * 1.2 }]);
    expect(wheels).toHaveLength(1);
    touch("touchmove", [{ x: 10, y: 500 - STEP * 2.1 }]);
    expect(wheels).toHaveLength(2);
  });

  it("a tap is left entirely alone, so it still focuses and types (AC-3)", () => {
    const start = touch("touchstart", [{ x: 40, y: 40 }]);
    const end = touch("touchend", [{ x: 40, y: 40 }]);

    expect(wheels).toHaveLength(0);
    // Preventing either is what suppresses the compatibility mouse events the
    // terminal needs to take focus and raise the keyboard.
    expect(start.defaultPrevented).toBe(false);
    expect(end.defaultPrevented).toBe(false);
  });

  it("a jitter under the tap slop is still a tap", () => {
    touch("touchstart", [{ x: 40, y: 40 }]);
    const moved = touch("touchmove", [{ x: 43, y: 44 }]);
    const end = touch("touchend", [{ x: 43, y: 44 }]);

    expect(wheels).toHaveLength(0);
    expect(moved.defaultPrevented).toBe(false);
    expect(end.defaultPrevented).toBe(false);
  });

  it("a sideways drag is left to the terminal's own selection", () => {
    const moved = (() => {
      touch("touchstart", [{ x: 10, y: 200 }]);
      return touch("touchmove", [{ x: 10 + STEP * 2, y: 210 }]);
    })();

    expect(wheels).toHaveLength(0);
    expect(moved.defaultPrevented).toBe(false);
  });

  it("a drag that scrolled swallows the click it would otherwise synthesize", () => {
    touch("touchstart", [{ x: 10, y: 400 }]);
    const moved = touch("touchmove", [{ x: 10, y: 400 - STEP }]);
    const end = touch("touchend", [{ x: 10, y: 400 - STEP }]);

    expect(moved.defaultPrevented).toBe(true);
    expect(end.defaultPrevented).toBe(true);
  });

  it("a second finger abandons the gesture rather than fighting a pinch", () => {
    touch("touchstart", [{ x: 10, y: 400 }]);
    touch("touchmove", [
      { x: 10, y: 400 - STEP },
      { x: 200, y: 400 + STEP },
    ]);

    expect(wheels).toHaveLength(0);
  });

  it("survives xterm stopping the touch it does not want", () => {
    // xterm's own touch handlers call `stopPropagation` whenever the running
    // app has not asked for mouse reporting. Listening in the bubble phase
    // would lose the gesture on exactly those servers.
    xterm.addEventListener("touchstart", (e) => e.stopPropagation());
    xterm.addEventListener("touchmove", (e) => e.stopPropagation());
    drag({ x: 10, y: 400 }, { x: 10, y: 400 - STEP });

    expect(wheels).toHaveLength(1);
  });

  it("detaching stops the terminal hearing touches at all", () => {
    detach();
    drag({ x: 10, y: 400 }, { x: 10, y: 400 - STEP * 2 });

    expect(wheels).toHaveLength(0);
  });
});
