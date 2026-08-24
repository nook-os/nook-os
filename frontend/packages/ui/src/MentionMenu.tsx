// The `@` picker's menu (MAIN-633).
//
// Presentational and unfocusable on purpose: the caret never leaves the editor,
// so arrows, Enter and Escape are the EDITOR's keymap delegating here, and
// there is no focus to return (AC-3). A menu that took focus would make every
// keystroke after it ambiguous and would fight the caret it is completing.
//
// Portalled to the body because the description editor lives inside two
// `overflow: auto` panels, which is the same reason `useAnchoredMenu` exists —
// but the anchor here is a caret coordinate, not an element's rect.
import React from "react";
import { createPortal } from "react-dom";

/** A workspace the menu can insert — the wire shape of `WorkspaceMention`. */
export interface MentionOption {
  workspace_id: string;
  name: string;
  slug: string;
}

/** Where a `@` menu sits, in viewport coordinates. */
export interface MentionAnchor {
  left: number;
  top: number;
}

export function MentionMenu({
  options,
  loading,
  query,
  active,
  anchor,
  onPick,
}: {
  options: MentionOption[];
  loading: boolean;
  query: string;
  /** Index into `options`, ignored when there are none. */
  active: number;
  anchor: MentionAnchor;
  onPick: (option: MentionOption) => void;
}) {
  const empty = !loading && options.length === 0;
  return createPortal(
    <div
      className="mention-menu"
      role="listbox"
      aria-label="workspaces"
      style={{ position: "fixed", left: anchor.left, top: anchor.top }}
    >
      {loading && options.length === 0 && (
        <div className="mention-row faint">searching…</div>
      )}
      {/* Never silently absent (AC-6): a menu that vanishes when nothing
          matches is indistinguishable from a feature that is broken, and the
          reader has no way to learn that the slug they typed is not a repo. */}
      {empty && (
        <div className="mention-row mention-empty faint">
          no workspace matches “{query}”
        </div>
      )}
      {options.map((o, i) => (
        <div
          key={o.workspace_id}
          role="option"
          aria-selected={i === active}
          className={`mention-row ${i === active ? "on" : ""}`}
          // `mouseDown`, not `click`: a click would first blur the editor, and
          // the blur closes the menu out from under the event.
          onMouseDown={(e) => {
            e.preventDefault();
            onPick(o);
          }}
        >
          <span className="mention-slug">@{o.slug}</span>
          <span className="mention-name faint">{o.name}</span>
        </div>
      ))}
    </div>,
    document.body,
  );
}
