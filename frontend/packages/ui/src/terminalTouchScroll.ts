// A finger drag over the terminal, translated into the wheel events xterm
// already forwards to tmux (MAIN-621).
//
// The translation is a real `wheel` event dispatched at xterm's own element,
// not a private call: with `mouse on` tmux asks xterm for wheel reports, xterm
// binds that listener itself, and `WheelUpPane` is what enters copy-mode (see
// `tmux.rs`). Synthesizing the event therefore lands in tmux's history exactly
// where the mouse wheel does — which is the only place history exists, since
// `scrollback: 0` means the browser holds none of it.
//
// Kept out of `TerminalView` because that component cannot be driven without a
// live renderer, while the part that can be wrong here — direction, thresholds,
// what counts as a tap — is pure arithmetic over touch points.

/** tmux moves three lines per wheel notch (`send-keys -N 3`, `tmux.rs`), so a
 *  notch every three rows makes the text track the finger at roughly 1:1. */
const ROWS_PER_NOTCH = 3;

/** Stands in until the terminal can report a row height (first frame, or a
 *  panel laid out with no height yet). Close to 13px type at 1.05 line-height. */
const FALLBACK_ROW_HEIGHT = 17;

/** Travel before a touch stops being a tap. Under it nothing is claimed, so
 *  the browser still synthesizes the mouse events that focus the terminal and
 *  raise the on-screen keyboard. */
const TAP_SLOP = 8;

export interface TouchScrollTargets {
  /** The element to dispatch the wheel at. It must be xterm's OWN element:
   *  xterm registers its wheel listener there, and an event dispatched at an
   *  ancestor never reaches a descendant's listener. */
  wheelTarget(): HTMLElement | null;
  /** CSS pixels per terminal row; 0 when it cannot be measured yet. */
  rowHeight(): number;
}

/** Wire touch scrolling onto `host`. Returns the detach function. */
export function attachTouchScroll(
  host: HTMLElement,
  targets: TouchScrollTargets,
): () => void {
  // The touch being followed. A second finger abandons the gesture rather than
  // fighting the browser for a pinch.
  let touchId: number | null = null;
  let startX = 0;
  let startY = 0;
  let lastY = 0;
  /** null until the drag has moved far enough to be classified. */
  let scrolling: boolean | null = null;
  /** Travel not yet worth a notch, carried between moves so a slow drag still
   *  scrolls instead of rounding to nothing every frame. */
  let carry = 0;

  const stepPx = () => {
    const row = targets.rowHeight();
    const rowPx = Number.isFinite(row) && row > 0 ? row : FALLBACK_ROW_HEIGHT;
    return rowPx * ROWS_PER_NOTCH;
  };

  const wheel = (deltaY: number, clientX: number, clientY: number) => {
    const target = targets.wheelTarget();
    if (!target) return;
    target.dispatchEvent(
      new WheelEvent("wheel", {
        deltaY,
        // Lines, not pixels: one line per unit whatever the font size, and no
        // dependence on xterm's internal pixel-to-row measurement.
        deltaMode: WheelEvent.DOM_DELTA_LINE,
        clientX,
        clientY,
        bubbles: true,
        cancelable: true,
      }),
    );
  };

  const onTouchStart = (ev: TouchEvent) => {
    if (ev.touches.length !== 1) {
      touchId = null;
      return;
    }
    const t = ev.touches[0];
    touchId = t.identifier;
    startX = t.clientX;
    startY = lastY = t.clientY;
    scrolling = null;
    carry = 0;
  };

  const onTouchMove = (ev: TouchEvent) => {
    if (touchId === null) return;
    if (ev.touches.length !== 1) {
      touchId = null;
      return;
    }
    const t = ev.touches[0];
    if (t.identifier !== touchId) return;

    if (scrolling === null) {
      const dx = Math.abs(t.clientX - startX);
      const dy = Math.abs(t.clientY - startY);
      if (Math.max(dx, dy) < TAP_SLOP) return;
      // A sideways drag stays the terminal's: with mouse reporting on that is
      // tmux's own selection, and swallowing it would trade one lost gesture
      // for another.
      scrolling = dy > dx;
    }
    if (!scrolling) return;

    // Claiming the gesture is also what stops the browser synthesizing a click
    // out of it — a drag that scrolled must not also arrive as a mouse press
    // in whatever the pane is running.
    ev.preventDefault();

    carry += lastY - t.clientY; // positive: the finger moved up
    lastY = t.clientY;
    const step = stepPx();
    const notches = Math.trunc(carry / step);
    if (notches === 0) return;
    carry -= notches * step;

    // Finger up is wheel UP, which is older output — the wheel's direction
    // (AC-1), not a document pan's.
    const deltaY = notches > 0 ? -1 : 1;
    // One event per notch: in mouse-reporting mode xterm emits one report per
    // wheel EVENT and reads the delta only for its sign, so a single event
    // carrying `deltaY: -4` would scroll tmux exactly as far as `-1`.
    for (let i = Math.abs(notches); i > 0; i--) wheel(deltaY, t.clientX, t.clientY);
  };

  const onTouchEnd = (ev: TouchEvent) => {
    if (scrolling && ev.cancelable) ev.preventDefault();
    touchId = null;
    scrolling = null;
    carry = 0;
  };

  // CAPTURE, not bubble. xterm has touch handlers of its own on its element,
  // and when the running app has NOT asked for mouse reporting they call
  // `stopPropagation` — so a bubble-phase listener on the host would never see
  // the touchstart, and the gesture would silently do nothing on exactly the
  // servers whose wheel is already misbehaving. Capturing runs this first and
  // leaves xterm's handlers to scroll the (empty) local viewport as before.
  const capture = { capture: true };
  host.addEventListener("touchstart", onTouchStart, { ...capture, passive: true });
  host.addEventListener("touchmove", onTouchMove, { ...capture, passive: false });
  host.addEventListener("touchend", onTouchEnd, { ...capture, passive: false });
  host.addEventListener("touchcancel", onTouchEnd, { ...capture, passive: false });

  return () => {
    host.removeEventListener("touchstart", onTouchStart, capture);
    host.removeEventListener("touchmove", onTouchMove, capture);
    host.removeEventListener("touchend", onTouchEnd, capture);
    host.removeEventListener("touchcancel", onTouchEnd, capture);
  };
}
