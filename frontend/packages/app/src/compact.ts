// Is the viewport a phone? The question CSS cannot answer for the components
// that need it: a media query decides where a box sits, but it cannot decide
// whether a control is in the DOM at all — and drawer chrome (a toggle, a
// scrim) must not merely be invisible above the breakpoint, it must not exist,
// or the scrim sits over the page eating clicks (MAIN-498 AC-7).
import { useEffect, useState } from "react";
import { COMPACT_WIDTH } from "./sessionNav";

/** The viewport's width, tracked across resizes. Without a `window` it answers
 *  desktop: the layout that assumes room is the safe one to be wrong about. */
export function useViewportWidth(): number {
  const [width, setWidth] = useState(() =>
    typeof window === "undefined" ? 1600 : window.innerWidth,
  );
  useEffect(() => {
    const onResize = () => setWidth(window.innerWidth);
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);
  return width;
}

/** MAIN-187's `compact` breakpoint, read the same way the stylesheet reads it —
 *  `<=` the shared token, so JS and CSS flip on the same pixel. */
export function useCompact(): boolean {
  return useViewportWidth() <= COMPACT_WIDTH;
}
