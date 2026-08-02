// Row actions — THE one way a table row carries controls (QOL sprint 2026-08).
//
// The audit that predates this found every table inventing its own: an
// icon-only trash in one, a "revoke" text button in the next, danger colouring
// applied or forgotten at random, right-alignment done by inline style when it
// was done at all. This is the system that replaces all of it:
//
//   1. ONE actions cell per row, always the LAST column, right-aligned by the
//      wrapper — never by inline style.
//   2. An action is an icon at control-grid size with the verb in `title`
//      (and aria-label). A column repeating "revoke revoke revoke" is noise —
//      icon + tooltip is the pattern. `label` exists for the few actions a row
//      genuinely leads with (terminal, share, use); same button, just wider.
//   3. Destructive is `danger`, sits LAST, and stands slightly apart.
//   4. Quiet until pointed at: resting actions are faint, hovering the row
//      brings them up, hovering the action shows its intent colour (accent,
//      or err for danger). Data reads first; controls appear when reached for.
//
// Adding a control to a row means reaching for RowAction. If RowAction cannot
// express it, the control probably does not belong in a row.

import React from "react";
import { Loader } from "lucide-react";
import type { LucideIcon } from "lucide-react";

export function RowActions({ children }: { children: React.ReactNode }) {
  return <span className="row-actions">{children}</span>;
}

export function RowAction({
  icon: Icon,
  title,
  label,
  danger = false,
  disabled = false,
  busy = false,
  onClick,
}: {
  icon: LucideIcon;
  /** The verb, as a person would say it — becomes tooltip and aria-label. */
  title: string;
  /** Only for actions a row leads with; destructive actions never carry one. */
  label?: string;
  danger?: boolean;
  disabled?: boolean;
  /** In-flight: the icon becomes a spinner and the button locks. */
  busy?: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      className={`row-action${danger ? " danger" : ""}${label ? " labeled" : ""}`}
      title={title}
      aria-label={title}
      disabled={disabled || busy}
      onClick={onClick}
    >
      {busy ? <Loader size={12} className="spin" /> : <Icon size={12} />}
      {label}
    </button>
  );
}
