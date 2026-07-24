// Chrome-style keyboard switching for the session tab strip (desktop only).
//
// The model is deliberately Chrome's, not VS Code's: position-based, wrapping,
// no overlay or MRU picker. Ctrl+Tab / Ctrl+Shift+Tab step through the visible
// strip; Ctrl/Cmd+1..8 jump to a position and Ctrl/Cmd+9 to the last tab.
//
// The cycle math is pure and lives here so it can be tested without a DOM, and
// so there is one source of truth for the order: the caller passes the SAME
// array the strip renders, so the keyboard order can never drift from what you
// see.
import { useEffect, useRef } from "react";
import { isDesktop } from "./desktop";

/** The tab id to switch to when moving `delta` (+1 next, -1 previous) through
 *  the visible ids, wrapping at both ends. Returns `null` when there is nowhere
 *  to go. With no active id (the sessions list is showing), next selects the
 *  first tab and previous the last — so Ctrl+Tab from the list lands you on a
 *  tab (AC-6). With a single active tab there is nothing to switch to. */
export function cycleTab(
  ids: string[],
  activeId: string | undefined,
  delta: number,
): string | null {
  if (ids.length === 0) return null;
  const cur = activeId ? ids.indexOf(activeId) : -1;
  if (cur === -1) {
    // No active tab in the visible set: step in from the appropriate end.
    return delta >= 0 ? ids[0] : ids[ids.length - 1];
  }
  if (ids.length < 2) return null; // one tab, and it's the active one
  const n = ids.length;
  const idx = (((cur + delta) % n) + n) % n;
  return ids[idx];
}

/** The tab id for a 1-based position. `9` means "the last tab" regardless of
 *  count, matching Chrome; a position past the number of tabs is a no-op
 *  (`null`). */
export function jumpTab(ids: string[], pos: number): string | null {
  if (ids.length === 0) return null;
  if (pos === 9) return ids[ids.length - 1];
  if (pos >= 1 && pos <= ids.length) return ids[pos - 1];
  return null;
}

const IS_MAC = (): boolean =>
  /mac/i.test(navigator.platform || navigator.userAgent);

/** Classify a keydown as one of the switch shortcuts and resolve its target.
 *  `matched` means "this is our shortcut" (so it should be consumed, even when
 *  there is no tab to move to); `id` is where to go, or null for a no-op. Pure
 *  and exported for testing — takes the event fields it needs, not the DOM. */
export function resolveSwitch(
  e: Pick<KeyboardEvent, "key" | "code" | "ctrlKey" | "metaKey" | "altKey" | "shiftKey">,
  ids: string[],
  activeId: string | undefined,
  isMac: boolean,
): { matched: boolean; id: string | null } {
  // Ctrl+Tab / Ctrl+Shift+Tab — both platforms (AC-1).
  if (e.key === "Tab" && e.ctrlKey && !e.metaKey && !e.altKey) {
    return { matched: true, id: cycleTab(ids, activeId, e.shiftKey ? -1 : 1) };
  }

  // Mac tab muscle memory: Cmd+Shift+] / Cmd+Shift+[ and Cmd+Option+→ / ←
  // (AC-5). Bracket bindings key off `code` because Shift rewrites `]`→`}`.
  if (isMac) {
    if (e.metaKey && e.shiftKey && !e.ctrlKey && !e.altKey) {
      if (e.code === "BracketRight") return { matched: true, id: cycleTab(ids, activeId, 1) };
      if (e.code === "BracketLeft") return { matched: true, id: cycleTab(ids, activeId, -1) };
    }
    if (e.metaKey && e.altKey && !e.ctrlKey && !e.shiftKey) {
      if (e.key === "ArrowRight") return { matched: true, id: cycleTab(ids, activeId, 1) };
      if (e.key === "ArrowLeft") return { matched: true, id: cycleTab(ids, activeId, -1) };
    }
  }

  // Number jump: Cmd on macOS, Ctrl elsewhere (AC-4). `code` avoids layouts
  // where the modified digit differs.
  const jumpMod = isMac ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey;
  if (jumpMod && !e.shiftKey && !e.altKey) {
    const m = /^Digit([1-9])$/.exec(e.code);
    if (m) return { matched: true, id: jumpTab(ids, Number(m[1])) };
  }

  return { matched: false, id: null };
}

/**
 * Install the switch shortcuts while a tab strip is on screen.
 *
 * Desktop only (AC-7): in the web build the browser owns Ctrl+Tab and Cmd+n and
 * we install nothing. The listener is on `window` in the **capture** phase so it
 * runs before xterm's own key handler on the focused terminal — on a match we
 * `preventDefault()` and `stopImmediatePropagation()`, so the keystroke switches
 * tabs instead of reaching the PTY (AC-3).
 *
 * `ids` and `activeId` are read through a ref so a changing tab list never
 * reinstalls the listener; only mount/unmount does.
 */
export function useTabHotkeys(
  ids: string[],
  activeId: string | undefined,
  navigate: (to: string) => void,
): void {
  const ref = useRef({ ids, activeId, navigate });
  ref.current = { ids, activeId, navigate };

  useEffect(() => {
    if (!isDesktop()) return; // AC-7: web build installs no bindings.
    const isMac = IS_MAC();
    const handler = (e: KeyboardEvent) => {
      const { ids, activeId, navigate } = ref.current;
      const { matched, id } = resolveSwitch(e, ids, activeId, isMac);
      if (!matched) return;
      // Consume the key even on a no-op so it never lands in the shell.
      e.preventDefault();
      e.stopImmediatePropagation();
      if (id && id !== activeId) navigate(`/sessions/${id}`);
    };
    window.addEventListener("keydown", handler, true);
    return () => window.removeEventListener("keydown", handler, true);
  }, []);
}
