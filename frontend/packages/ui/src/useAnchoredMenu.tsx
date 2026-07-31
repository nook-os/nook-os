// Popups that are not clipped by whatever they happen to be inside.
//
// An absolutely-positioned menu is clipped by the nearest ancestor with
// `overflow`, and almost every menu in this app lives inside one: `.modal`
// (hidden), `.task-main` and `.task-side` (auto), `.op-table-wrap` (auto). The
// menu rendered *behind* those edges and could not be clicked.
//
// The fix is the same everywhere — render into `document.body` at
// `position: fixed`, tracking the trigger's rect — so it is a hook rather than
// something each menu re-derives. `Select` and the board's label picker use it;
// anything else opening a popup inside a panel should too.
import React, {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import { createPortal } from "react-dom";

/** Where the popup sits, in viewport coordinates. */
interface Placement {
  left: number;
  top: number;
  width: number;
  /** Opening upward flips the animation origin and the rounded corners. */
  up: boolean;
}

export interface AnchoredMenuOptions {
  /** Roughly how tall the menu wants to be, used to decide up vs down. */
  height?: number;
  /** Match the trigger's width (a select) rather than sizing to content. */
  matchWidth?: boolean;
  /**
   * Roughly how WIDE the menu is, when it is wider than the thing that opens it.
   *
   * The clamp that keeps a menu on screen has only the trigger to measure, which
   * is right for a select (the menu is the trigger's width) and wrong for a menu
   * hung off a 22px icon button: clamped by 22px, a 132px menu still runs off the
   * right edge. Defaults to the trigger's width, so every existing caller keeps
   * exactly the placement it had.
   */
  width?: number;
}

const DEFAULT_HEIGHT = 240;

/**
 * Anchor a popup to a trigger, outside every ancestor's overflow.
 *
 * Returns the ref to put on the trigger's wrapper, the ref for the menu, and
 * `portal()` — which renders its children into `document.body`, positioned, or
 * `null` when closed.
 */
export function useAnchoredMenu(
  open: boolean,
  close: () => void,
  { height = DEFAULT_HEIGHT, matchWidth = false, width }: AnchoredMenuOptions = {},
) {
  const [place, setPlace] = useState<Placement | null>(null);
  const hostRef = useRef<HTMLDivElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);

  const measure = useCallback(() => {
    const el = hostRef.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    // Open upward when there is not room below — a picker near the bottom of a
    // sidebar would otherwise render its options off the viewport.
    const up = window.innerHeight - r.bottom < height && r.top > height;
    const w = width ?? r.width;
    setPlace({
      left: Math.max(4, Math.min(r.left, window.innerWidth - w - 4)),
      top: up ? r.top - 3 : r.bottom + 3,
      width: r.width,
      up,
    });
  }, [height, width]);

  useLayoutEffect(() => {
    if (open) measure();
  }, [open, measure]);

  useEffect(() => {
    if (!open) return;

    // `capture: true` so ANY ancestor scrolling repositions it, not just the
    // window — the whole reason this is a portal is that those ancestors
    // scroll independently.
    const track = () => measure();
    window.addEventListener("scroll", track, true);
    window.addEventListener("resize", track);

    const away = (e: MouseEvent) => {
      const t = e.target as Node;
      // The menu is NOT inside the host any more, so both have to be checked
      // or clicking an option would close the menu before it fired.
      if (!hostRef.current?.contains(t) && !menuRef.current?.contains(t)) {
        close();
      }
    };
    // Capture phase: a modal (or any ancestor) that stops mousedown propagation
    // would otherwise swallow the click before this window-level listener saw it,
    // and the menu would never close on an outside click. Capture runs first.
    window.addEventListener("mousedown", away, true);

    return () => {
      window.removeEventListener("scroll", track, true);
      window.removeEventListener("resize", track);
      window.removeEventListener("mousedown", away, true);
    };
  }, [open, measure, close]);

  const portal = useCallback(
    (
      children: React.ReactNode,
      className: string,
      props: React.HTMLAttributes<HTMLDivElement> = {},
    ) => {
      if (!open || !place) return null;
      return createPortal(
        <div
          {...props}
          ref={menuRef}
          className={`${className} ${place.up ? "up" : "down"}`}
          style={{
            position: "fixed",
            left: place.left,
            ...(matchWidth ? { width: place.width } : {}),
            // Anchored by its bottom edge when opening upward, so it grows
            // away from the trigger rather than over it.
            ...(place.up
              ? { bottom: window.innerHeight - place.top }
              : { top: place.top }),
            ...props.style,
          }}
        >
          {children}
        </div>,
        document.body,
      );
    },
    [open, place, matchWidth],
  );

  return { hostRef, menuRef, up: place?.up ?? false, portal };
}
