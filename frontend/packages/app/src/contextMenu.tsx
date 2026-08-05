// App-wide context menu system (MAIN-167).
//
// NookOS presents as a native full-screen application, so the browser's own
// right-click menu should never appear. This module installs a SINGLE
// capture-phase `contextmenu` listener at the app root that ALWAYS calls
// `preventDefault()` — no path lets the native menu render — and then resolves
// what to open:
//
//   1. the NEAREST registered region wins (components declare items for their
//      DOM subtree with <ContextMenuRegion>), else
//   2. a contextual FALLBACK (Copy / Cut / Paste), computed from the target and
//      never empty.
//
// An element (or subtree) can also opt OUT with the `data-ctxmenu-native`
// attribute: the five legacy hand-rolled menus (TaskMenu, ControlPlanePill,
// ControlPlaneTabs, SessionTabs, SessionWindows) still open their OWN menu on
// contextmenu, so we mark their anchors and merely suppress the shared menu
// there — we still preventDefault, we just let their handler win. A follow-up
// ticket migrates those onto this system.
//
// Documented limits (out of a web page's reach, by browser design):
//   - Firefox reserves Shift+right-click for the native menu — unblockable; on
//     Firefox that gesture shows the browser menu regardless of this listener.
//   - Devtools / browser-chrome context menus are not page content and cannot
//     be intercepted here.
import React, {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import { createPortal } from "react-dom";
import { useLocation } from "react-router-dom";

/** One row of a context menu. A separator carries no label/onSelect. */
export interface ContextMenuItem {
  label?: string;
  onSelect?: () => void;
  disabled?: boolean;
  /** Right-aligned hint, e.g. a shortcut or a reason an item is disabled. */
  hint?: string;
  separator?: boolean;
  /** Destructive action — rendered in the error colour (Delete, Close Terminal). */
  danger?: boolean;
  /** Optional leading icon (e.g. a visibility glyph). Presentational only. */
  icon?: React.ReactNode;
  /** A submenu: hovering (or activating) this row opens a nested panel of these
   *  items to the side. One level only — children are leaf rows. Migrated from
   *  TaskMenu's hand-rolled hover submenus (MAIN-168). */
  children?: ContextMenuItem[];
}

/** Items for a region, or a function returning them at open time (so a region
 *  can read live state — a terminal's current selection — on each right-click). */
export type ItemSource = ContextMenuItem[] | (() => ContextMenuItem[]);

/** Marks a subtree whose own contextmenu handler opens a menu; the shared menu
 *  suppresses itself there (still preventing the native menu). */
export const CTX_NATIVE_ATTR = "data-ctxmenu-native";

/** Spread onto a legacy anchor that owns its own right-click menu. */
export const nativeContextMenu: Record<string, string> = { [CTX_NATIVE_ATTR]: "" };

// ── Region registry ────────────────────────────────────────────────────────
// Keyed by the region's DOM element. Resolution walks up from the event target
// and the first element found in this map wins — that is "nearest region".
const regions = new Map<HTMLElement, () => ItemSource>();

function resolveSource(source: ItemSource): ContextMenuItem[] {
  return typeof source === "function" ? source() : source;
}

// ── Clipboard-read capability ──────────────────────────────────────────────
// We must decide Paste's enabled state SYNCHRONOUSLY when the menu opens, but
// whether the browser will grant a clipboard read is only knowable async (and
// Firefox refuses it outright). Probe once via the Permissions API and cache
// the answer; fall back to a feature check until the probe resolves.
let clipboardRead: boolean | null = null;

export function probeClipboardRead(): void {
  const perms = navigator.permissions;
  if (!perms?.query) {
    clipboardRead = null;
    return;
  }
  perms
    .query({ name: "clipboard-read" as PermissionName })
    .then((status) => {
      clipboardRead = status.state !== "denied";
      status.onchange = () => {
        clipboardRead = status.state !== "denied";
      };
    })
    .catch(() => {
      // The Permissions API doesn't recognize 'clipboard-read' (Firefox): treat
      // clipboard reads as blocked so Paste renders disabled rather than
      // throwing on click.
      clipboardRead = false;
    });
}

/** Best current knowledge of whether `navigator.clipboard.readText()` will work. */
export function canReadClipboardNow(): boolean {
  if (clipboardRead !== null) return clipboardRead;
  return !!navigator.clipboard && typeof navigator.clipboard.readText === "function";
}

// ── Target inspection ──────────────────────────────────────────────────────
function editableFrom(target: EventTarget | null): HTMLElement | null {
  let el = target instanceof HTMLElement ? target : null;
  while (el) {
    const tag = el.tagName.toLowerCase();
    if (tag === "textarea" || tag === "input") return el;
    if (el.isContentEditable) return el;
    el = el.parentElement;
  }
  return null;
}

function inputFrom(
  target: EventTarget | null,
): HTMLInputElement | HTMLTextAreaElement | null {
  const ed = editableFrom(target);
  if (ed && (ed.tagName === "INPUT" || ed.tagName === "TEXTAREA")) {
    return ed as HTMLInputElement | HTMLTextAreaElement;
  }
  return null;
}

/** The selected text — from a form field's own range, or the window selection. */
function selectionText(target: EventTarget | null): string {
  const inp = inputFrom(target);
  if (inp) {
    const s = inp.selectionStart ?? 0;
    const e = inp.selectionEnd ?? 0;
    return inp.value.slice(s, e);
  }
  return window.getSelection()?.toString() ?? "";
}

function doCopy(target: EventTarget | null): void {
  const text = selectionText(target);
  if (text) void navigator.clipboard?.writeText(text).catch(() => {});
}

function doCut(target: EventTarget | null): void {
  const text = selectionText(target);
  if (!text) return;
  void navigator.clipboard?.writeText(text).catch(() => {});
  const inp = inputFrom(target);
  if (inp) {
    const s = inp.selectionStart ?? 0;
    const e = inp.selectionEnd ?? 0;
    inp.setRangeText("", s, e, "end");
    inp.dispatchEvent(new Event("input", { bubbles: true }));
  } else {
    window.getSelection()?.deleteFromDocument();
  }
}

async function doPaste(target: EventTarget | null): Promise<void> {
  let text = "";
  try {
    text = await navigator.clipboard.readText();
  } catch {
    // Refused (e.g. Firefox) — Paste should already have been disabled; bail.
    return;
  }
  if (!text) return;
  const inp = inputFrom(target);
  if (inp) {
    const s = inp.selectionStart ?? inp.value.length;
    const e = inp.selectionEnd ?? inp.value.length;
    inp.setRangeText(text, s, e, "end");
    inp.dispatchEvent(new Event("input", { bubbles: true }));
    return;
  }
  const sel = window.getSelection();
  if (sel && sel.rangeCount) {
    const range = sel.getRangeAt(0);
    range.deleteContents();
    const node = document.createTextNode(text);
    range.insertNode(node);
    range.setStartAfter(node);
    range.collapse(true);
    sel.removeAllRanges();
    sel.addRange(range);
    if (target instanceof HTMLElement) {
      target.dispatchEvent(new Event("input", { bubbles: true }));
    }
  }
}

// ── Contextual fallback (AC-3) ─────────────────────────────────────────────
export interface FallbackContext {
  hasSelection: boolean;
  isEditable: boolean;
  canReadClipboard: boolean;
}

export interface FallbackActions {
  copy?: () => void;
  cut?: () => void;
  paste?: () => void;
}

/**
 * The fallback menu as a PURE function of the three inputs — the load-bearing
 * enablement matrix (selection × editable × clipboard-permission), factored out
 * so it can be unit-tested without a DOM (AC-7). Actions are injected; with none
 * supplied the items are inert (what the tests inspect).
 *
 *   - Copy   — always shown; enabled only when there is a non-empty selection.
 *   - Cut    — editable only; enabled only when there is a selection.
 *   - Paste  — editable only; disabled with a hint when the clipboard can't be
 *              read, rather than throwing on click.
 */
export function fallbackItems(
  ctx: FallbackContext,
  actions: FallbackActions = {},
): ContextMenuItem[] {
  const items: ContextMenuItem[] = [
    {
      label: "Copy",
      disabled: !ctx.hasSelection,
      onSelect: actions.copy,
    },
  ];
  if (ctx.isEditable) {
    items.push({
      label: "Cut",
      disabled: !ctx.hasSelection,
      onSelect: actions.cut,
    });
    items.push(
      ctx.canReadClipboard
        ? { label: "Paste", onSelect: actions.paste }
        : { label: "Paste", disabled: true, hint: "Clipboard access blocked" },
    );
  }
  return items;
}

function buildFallback(target: EventTarget | null): ContextMenuItem[] {
  return fallbackItems(
    {
      hasSelection: selectionText(target).length > 0,
      isEditable: !!editableFrom(target),
      canReadClipboard: canReadClipboardNow(),
    },
    {
      copy: () => doCopy(target),
      cut: () => doCut(target),
      paste: () => void doPaste(target),
    },
  );
}

/** Walk up from a target: nearest region → its items, a native-owner → suppress,
 *  otherwise the fallback. Exported for tests. */
export function resolveItems(
  target: EventTarget | null,
): ContextMenuItem[] | "native" {
  let el = target instanceof HTMLElement ? target : null;
  while (el) {
    if (el.hasAttribute(CTX_NATIVE_ATTR)) return "native";
    const region = regions.get(el);
    if (region) return resolveSource(region());
    el = el.parentElement;
  }
  return buildFallback(target);
}

// ── Provider + open state ──────────────────────────────────────────────────
interface OpenState {
  x: number;
  y: number;
  items: ContextMenuItem[];
}

interface ContextMenuApi {
  /** Open at a point in viewport coordinates with an explicit item list. */
  openAt(x: number, y: number, items: ContextMenuItem[]): void;
  close(): void;
}

const Ctx = createContext<ContextMenuApi | null>(null);

export function useContextMenuApi(): ContextMenuApi {
  const api = useContext(Ctx);
  if (!api) throw new Error("useContextMenuApi outside <ContextMenuProvider>");
  return api;
}

export function ContextMenuProvider({ children }: { children: React.ReactNode }) {
  const [open, setOpen] = useState<OpenState | null>(null);
  // Restore focus to whatever was focused before the menu grabbed it (AC-5).
  const restoreFocus = useRef<HTMLElement | null>(null);
  const location = useLocation();

  // Probe once whether the browser will grant a clipboard read, so Paste's
  // enabled state is correct from the first right-click.
  useEffect(() => probeClipboardRead(), []);

  const close = useCallback(() => setOpen(null), []);
  const openAt = useCallback((x: number, y: number, items: ContextMenuItem[]) => {
    if (items.length === 0) return;
    restoreFocus.current = document.activeElement as HTMLElement | null;
    setOpen({ x, y, items });
  }, []);

  // AC-1: one capture-phase listener, always preventDefault, resolves the menu.
  useEffect(() => {
    const onContextMenu = (e: MouseEvent) => {
      e.preventDefault();
      const resolved = resolveItems(e.target);
      if (resolved === "native") {
        // A legacy island owns this gesture; let its own handler open its menu.
        setOpen(null);
        return;
      }
      openAt(e.clientX, e.clientY, resolved);
    };
    document.addEventListener("contextmenu", onContextMenu, true);
    return () => document.removeEventListener("contextmenu", onContextMenu, true);
  }, [openAt]);

  // MAIN-418 AC-4: LONG-PRESS opens the same menu on touch.
  //
  // A phone has no right-click, so every action that lives only in a context
  // menu is unreachable without this — and after MAIN-416 that includes rename
  // and stop, which are the pane's only actions. Long-press is the platform
  // gesture for it on both iOS and Android.
  //
  // ~500ms, cancelled by movement (that is a scroll, not a press) and by the
  // finger lifting early. `touchend` is not needed to cancel the timer — the
  // press has already fired by then or been cleared — but a MOVE must cancel,
  // or scrolling a long list pops a menu under your thumb.
  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | null = null;
    let startedAt: { x: number; y: number } | null = null;

    const clear = () => {
      if (timer) clearTimeout(timer);
      timer = null;
      startedAt = null;
    };

    const onStart = (e: TouchEvent) => {
      if (e.touches.length !== 1) return; // a pinch or a second finger is not a press
      const t = e.touches[0];
      startedAt = { x: t.clientX, y: t.clientY };
      const target = e.target;
      timer = setTimeout(() => {
        const resolved = resolveItems(target);
        // A legacy island still owns its own gesture; do not open two menus.
        if (resolved === "native") return;
        openAt(t.clientX, t.clientY, resolved);
      }, 500);
    };

    const onMove = (e: TouchEvent) => {
      if (!startedAt || !e.touches.length) return;
      const t = e.touches[0];
      // 10px of slop: a finger resting on a screen is never perfectly still.
      if (Math.abs(t.clientX - startedAt.x) > 10 || Math.abs(t.clientY - startedAt.y) > 10) {
        clear();
      }
    };

    document.addEventListener("touchstart", onStart, { passive: true });
    document.addEventListener("touchmove", onMove, { passive: true });
    document.addEventListener("touchend", clear, { passive: true });
    document.addEventListener("touchcancel", clear, { passive: true });
    return () => {
      clear();
      document.removeEventListener("touchstart", onStart);
      document.removeEventListener("touchmove", onMove);
      document.removeEventListener("touchend", clear);
      document.removeEventListener("touchcancel", clear);
    };
  }, [openAt]);

  // AC-5: keyboard invocation — Shift+F10 and the ContextMenu (Menu) key open
  // the menu at the focused element, positioned by its bounding rect.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const isMenuKey =
        e.key === "ContextMenu" || (e.shiftKey && e.key === "F10");
      if (!isMenuKey) return;
      const focused = document.activeElement as HTMLElement | null;
      const target = focused ?? document.body;
      const resolved = resolveItems(target);
      if (resolved === "native") return;
      e.preventDefault();
      const r = focused?.getBoundingClientRect();
      openAt(r ? r.left : 0, r ? r.bottom : 0, resolved);
    };
    document.addEventListener("keydown", onKey, true);
    return () => document.removeEventListener("keydown", onKey, true);
  }, [openAt]);

  // Dismiss on scroll (capture, so any scroll container counts).
  useEffect(() => {
    if (!open) return;
    const onScroll = () => close();
    window.addEventListener("scroll", onScroll, true);
    return () => window.removeEventListener("scroll", onScroll, true);
  }, [open, close]);

  // Dismiss on route change.
  useEffect(() => {
    setOpen(null);
  }, [location.pathname]);

  const api: ContextMenuApi = { openAt, close };

  return (
    <Ctx.Provider value={api}>
      {children}
      {open && (
        <ContextMenu
          state={open}
          onClose={() => {
            const el = restoreFocus.current;
            setOpen(null);
            // Return focus where the user was (AC-5).
            if (el && document.contains(el)) el.focus();
          }}
        />
      )}
    </Ctx.Provider>
  );
}

// ── The menu itself (AC-2 / AC-5) ──────────────────────────────────────────
function ContextMenu({
  state,
  onClose,
}: {
  state: OpenState;
  onClose: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState({ x: state.x, y: state.y });
  // Indices of activatable (non-separator, non-disabled) items, for arrow nav.
  // Parents (with children) ARE navigable — activating one opens its submenu.
  const navigable = state.items
    .map((it, i) => ({ it, i }))
    .filter(({ it }) => !it.separator && !it.disabled)
    .map(({ i }) => i);
  const [active, setActive] = useState<number>(-1);
  // The top-level index whose submenu is open (or -1), and the focused child
  // within it. A submenu is mouse-hover / click driven (as TaskMenu's were),
  // with keyboard entry via Enter/→ (MAIN-168).
  const [openSub, setOpenSub] = useState<number>(-1);
  const [subActive, setSubActive] = useState<number>(-1);
  const itemRefs = useRef<(HTMLButtonElement | null)[]>([]);
  const subRefs = useRef<(HTMLButtonElement | null)[]>([]);

  // Clamp into the viewport at the pointer — same approach as TaskMenu, so a
  // menu opened near an edge never renders half off-screen.
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    setPos({
      x: Math.min(state.x, window.innerWidth - r.width - 8),
      y: Math.min(state.y, window.innerHeight - r.height - 8),
    });
  }, [state]);

  // Move focus into the menu on open (AC-5).
  useLayoutEffect(() => {
    ref.current?.focus();
  }, []);

  // Dismiss on click-away and Escape; keyboard navigation within.
  useEffect(() => {
    // `mousedown` (not `click`): a listener added during the opening event would
    // otherwise fire on that very event and close as it opens.
    const away = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) onClose();
    };
    window.addEventListener("mousedown", away, true);
    return () => window.removeEventListener("mousedown", away, true);
  }, [onClose]);

  // Run a leaf item: close the menu, then fire its action.
  const runLeaf = (item: ContextMenuItem | undefined) => {
    if (!item || item.disabled || item.separator || item.children) return;
    onClose();
    item.onSelect?.();
  };

  // Open a top item's submenu and focus its first activatable child.
  const openSubmenu = (i: number) => {
    const kids = state.items[i]?.children ?? [];
    const first = kids.findIndex((k) => !k.separator && !k.disabled);
    setOpenSub(i);
    setSubActive(first);
    if (first >= 0) requestAnimationFrame(() => subRefs.current[first]?.focus());
  };

  const closeSubmenu = () => {
    const parent = openSub;
    setOpenSub(-1);
    setSubActive(-1);
    if (parent >= 0) itemRefs.current[parent]?.focus();
  };

  // Activate the focused TOP item: a parent toggles its submenu, a leaf runs.
  const activateTop = (i: number) => {
    const item = state.items[i];
    if (!item || item.disabled || item.separator) return;
    if (item.children?.length) {
      openSub === i ? closeSubmenu() : openSubmenu(i);
    } else {
      runLeaf(item);
    }
  };

  const moveTop = (dir: 1 | -1) => {
    if (navigable.length === 0) return;
    setOpenSub(-1);
    setSubActive(-1);
    setActive((cur) => {
      const at = navigable.indexOf(cur);
      const next =
        at === -1
          ? dir === 1
            ? navigable[0]
            : navigable[navigable.length - 1]
          : navigable[(at + dir + navigable.length) % navigable.length];
      itemRefs.current[next]?.focus();
      return next;
    });
  };

  // Navigate within the open submenu's activatable children.
  const moveSub = (dir: 1 | -1) => {
    if (openSub < 0) return;
    const kids = state.items[openSub]?.children ?? [];
    const nav = kids
      .map((k, j) => ({ k, j }))
      .filter(({ k }) => !k.separator && !k.disabled)
      .map(({ j }) => j);
    if (nav.length === 0) return;
    setSubActive((cur) => {
      const at = nav.indexOf(cur);
      const next =
        at === -1
          ? dir === 1
            ? nav[0]
            : nav[nav.length - 1]
          : nav[(at + dir + nav.length) % nav.length];
      subRefs.current[next]?.focus();
      return next;
    });
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    const inSub = openSub >= 0 && subActive >= 0;
    switch (e.key) {
      case "Escape":
        e.preventDefault();
        if (openSub >= 0) closeSubmenu();
        else onClose();
        break;
      case "ArrowDown":
        e.preventDefault();
        inSub ? moveSub(1) : moveTop(1);
        break;
      case "ArrowUp":
        e.preventDefault();
        inSub ? moveSub(-1) : moveTop(-1);
        break;
      case "ArrowRight":
        if (!inSub && active >= 0 && state.items[active]?.children?.length) {
          e.preventDefault();
          openSubmenu(active);
        }
        break;
      case "ArrowLeft":
        if (openSub >= 0) {
          e.preventDefault();
          closeSubmenu();
        }
        break;
      case "Home":
      case "End":
        if (!inSub && navigable.length) {
          e.preventDefault();
          const idx = e.key === "Home" ? navigable[0] : navigable[navigable.length - 1];
          setOpenSub(-1);
          setActive(idx);
          itemRefs.current[idx]?.focus();
        }
        break;
      case "Enter":
      case " ":
        e.preventDefault();
        if (inSub) runLeaf(state.items[openSub]?.children?.[subActive]);
        else if (active >= 0) activateTop(active);
        break;
    }
  };

  const row = (item: ContextMenuItem, i: number, hasSub: boolean) => (
    <>
      <span className="ctxmenu-label">
        {item.icon && <span className="ctxmenu-icon">{item.icon}</span>}
        {item.label}
      </span>
      {item.hint && <span className="ctxmenu-hint">{item.hint}</span>}
      {hasSub && <span className="ctxmenu-arrow" aria-hidden="true">›</span>}
    </>
  );

  return createPortal(
    <div
      ref={ref}
      className="ctxmenu"
      role="menu"
      tabIndex={-1}
      style={{ left: pos.x, top: pos.y }}
      onKeyDown={onKeyDown}
      onContextMenu={(e) => e.preventDefault()}
      onMouseDown={(e) => e.stopPropagation()}
    >
      {state.items.map((item, i) => {
        if (item.separator) {
          return <div key={`sep-${i}`} className="ctxmenu-sep" role="separator" />;
        }
        const hasSub = !!item.children?.length;
        return (
          <div
            key={`${item.label}-${i}`}
            className="ctxmenu-subhost"
            onMouseEnter={() => {
              setActive(i);
              setOpenSub(hasSub ? i : -1);
              setSubActive(-1);
            }}
          >
            <button
              ref={(el) => {
                itemRefs.current[i] = el;
              }}
              className={`ctxmenu-item${item.danger ? " danger" : ""}`}
              role="menuitem"
              type="button"
              disabled={item.disabled}
              aria-disabled={item.disabled || undefined}
              aria-haspopup={hasSub || undefined}
              aria-expanded={hasSub ? openSub === i : undefined}
              tabIndex={-1}
              onMouseDown={(e) => e.stopPropagation()}
              onClick={() => (hasSub ? activateTop(i) : runLeaf(item))}
            >
              {row(item, i, hasSub)}
            </button>
            {hasSub && openSub === i && (
              <div className="ctxmenu-submenu" role="menu">
                {item.children!.map((child, j) =>
                  child.separator ? (
                    <div key={`sub-sep-${j}`} className="ctxmenu-sep" role="separator" />
                  ) : (
                    <button
                      key={`${child.label}-${j}`}
                      ref={(el) => {
                        subRefs.current[j] = el;
                      }}
                      className={`ctxmenu-item${child.danger ? " danger" : ""}`}
                      role="menuitem"
                      type="button"
                      disabled={child.disabled}
                      aria-disabled={child.disabled || undefined}
                      tabIndex={-1}
                      onMouseEnter={() => setSubActive(j)}
                      onMouseDown={(e) => e.stopPropagation()}
                      onClick={() => runLeaf(child)}
                    >
                      {row(child, j, false)}
                    </button>
                  ),
                )}
              </div>
            )}
          </div>
        );
      })}
    </div>,
    document.body,
  );
}

// ── Region API (AC-2) ──────────────────────────────────────────────────────
/**
 * Declare context-menu items for a DOM subtree. Right-clicking anywhere inside
 * opens these items instead of the fallback; the NEAREST region wins when they
 * nest. `items` may be a function, evaluated at open time, for live state.
 *
 * Renders a plain wrapper element; pass `style={{ display: "contents" }}` to
 * keep it transparent to layout (as the session terminal does).
 */
export function ContextMenuRegion({
  items,
  children,
  className,
  style,
}: {
  items: ItemSource;
  children: React.ReactNode;
  className?: string;
  style?: React.CSSProperties;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const itemsRef = useRef<ItemSource>(items);
  itemsRef.current = items;

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    regions.set(el, () => itemsRef.current);
    return () => {
      regions.delete(el);
    };
  }, []);

  return (
    <div ref={ref} className={className} style={style}>
      {children}
    </div>
  );
}
