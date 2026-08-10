// The composer's emoji picker (MAIN-171 AC-1).
//
// Frontend-only and offline: a fixed list of unicode characters, no service and
// no image assets. Picking one inserts it at the caret — the caller owns the
// text, so this component only reports which character was chosen.
//
// Keyboard-complete, because a picker you can only click is not usable from a
// composer you reached by typing: the trigger opens it, arrows walk the grid,
// Enter (or Space) inserts, Escape closes and hands focus back to the trigger.
import React, { useCallback, useEffect, useRef, useState } from "react";
import { Smile } from "lucide-react";
import { useAnchoredMenu } from "./useAnchoredMenu";

/**
 * The offered characters, roughly grouped (faces, gestures, hearts, objects,
 * marks). Curated rather than exhaustive: a full emoji keyboard is a component
 * of its own, and what a chat composer needs is the handful people reach for.
 */
export const COMPOSER_EMOJI = [
  "😀", "😄", "😅", "🤣", "🙂", "😉", "😍", "🤩",
  "🤔", "🫡", "😴", "😭", "😤", "😱", "🤯", "🥳",
  "👍", "👎", "👏", "🙌", "🙏", "👋", "🤝", "💪",
  "❤️", "🧡", "💛", "💚", "💙", "💜", "💔", "✨",
  "🔥", "🎉", "🚀", "⭐", "⚡", "🌈", "☕", "🍕",
  "👀", "🧠", "🐛", "🔧", "📦", "📈", "⏰", "🧪",
  "✅", "❌", "⚠️", "❓", "❗", "💯", "🤖", "🎯",
] as const;

/** How many sit on one row — arrow up/down move by exactly this. */
const COLUMNS = 8;

/** Matches `.chat-emoji-picker` in the stylesheet, so it flips up near the
 *  bottom of the window instead of opening off-screen. */
const PICKER_HEIGHT = 232;
const PICKER_WIDTH = 264;

export interface EmojiPickerProps {
  /** The chosen character. The caller inserts it wherever its caret is. */
  onPick: (emoji: string) => void;
  disabled?: boolean;
}

export function EmojiPicker({ onPick, disabled = false }: EmojiPickerProps) {
  const [open, setOpen] = useState(false);
  const trigger = useRef<HTMLButtonElement>(null);
  const gridRef = useRef<HTMLDivElement>(null);

  const close = useCallback(() => setOpen(false), []);
  const menu = useAnchoredMenu(open, close, {
    height: PICKER_HEIGHT,
    width: PICKER_WIDTH,
  });

  /** Close and put the caret back where the person left it. */
  const dismiss = useCallback(() => {
    setOpen(false);
    trigger.current?.focus();
  }, []);

  // Escape closes from wherever focus happens to be, not only from inside the
  // grid. The grid is portalled into `document.body`, so a handler on it sees a
  // key only while focus is in there — and focus can legitimately be elsewhere
  // (the caret went back to the composer after the last pick). A popup you can
  // open but not dismiss with Escape is the failure that guards against.
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.preventDefault();
      dismiss();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [open, dismiss]);

  // Roving focus across the grid. The options are plain buttons in DOM order,
  // so "one row down" is "eight buttons along" — no coordinate bookkeeping, and
  // nothing to keep in step with the CSS beyond the column count.
  const onGridKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      const buttons = [...(gridRef.current?.querySelectorAll("button") ?? [])];
      const at = buttons.indexOf(document.activeElement as HTMLButtonElement);
      if (at < 0) return;
      const step =
        e.key === "ArrowRight" ? 1
        : e.key === "ArrowLeft" ? -1
        : e.key === "ArrowDown" ? COLUMNS
        : e.key === "ArrowUp" ? -COLUMNS
        : 0;
      if (step === 0) return;
      e.preventDefault();
      const next = at + step;
      // Clamp rather than wrap: at an edge the focus stays put, which is what
      // every native grid does and what stops an ArrowDown on the last row
      // jumping back to the first.
      if (next >= 0 && next < buttons.length) buttons[next].focus();
    },
    [],
  );

  return (
    <div ref={menu.hostRef} className="chat-composer-wrap">
      <button
        ref={trigger}
        type="button"
        className={`chat-composer-btn${open ? " on" : ""}`}
        title="Insert emoji"
        aria-label="Insert emoji"
        aria-haspopup="menu"
        aria-expanded={open}
        disabled={disabled}
        onClick={() => setOpen((v) => !v)}
      >
        <Smile size={15} />
      </button>
      {menu.portal(
        <div ref={gridRef} className="chat-emoji-grid">
          {COMPOSER_EMOJI.map((emoji, i) => (
            <button
              key={emoji}
              type="button"
              role="menuitem"
              className="chat-emoji-opt"
              // Opening has to land focus inside the grid, or a keyboard user
              // reaches the trigger and then has nowhere to arrow from — and
              // Escape has no focus to hand back.
              autoFocus={i === 0}
              aria-label={`Insert ${emoji}`}
              onClick={() => {
                dismiss();
                onPick(emoji);
              }}
            >
              {emoji}
            </button>
          ))}
        </div>,
        "chat-emoji-picker",
        { role: "menu", "aria-label": "Emoji", onKeyDown: onGridKeyDown },
      )}
    </div>
  );
}
