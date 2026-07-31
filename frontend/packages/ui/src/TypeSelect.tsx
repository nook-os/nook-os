// The issue-type control, shared by the task modal and the board filter
// (MAIN-174).
//
// It began as a private component in `TaskDetail`, while the board's Type filter
// was a row of toggle chips built separately. Two controls for one concept, and
// they neither looked nor behaved alike — the filter row read as a different
// application from the modal it filters.
//
// So there is one component with two modes, rather than two components:
//
//   single   — the modal. Pick a type, the menu closes, `onChange` fires with
//              the new type. Exactly what it did before; the modal's call site
//              did not change (NG-3).
//   multiple — the filter. Each item TOGGLES membership and the menu stays open,
//              because choosing "bug or chore" is one gesture, not two visits.
//              `onChange` gets the whole array, preserving the filter's OR
//              semantics unchanged (NG-1).
//
// The modes are a discriminated union rather than two optional prop pairs, so a
// caller cannot supply a `string` where the array is meant, and TypeScript
// narrows `onChange`'s argument for them.
import React, { useCallback, useState } from "react";
import { ChevronDown } from "lucide-react";
import { TYPE_META } from "./components";
import { useAnchoredMenu } from "./useAnchoredMenu";

interface Common {
  /** Overrides the trigger's `aria-label`; defaults to describing the value. */
  ariaLabel?: string;
}

export type TypeSelectProps = Common &
  (
    | {
        multiple?: false;
        /** The current type; `null`/absent reads as `task`. */
        value: string | null | undefined;
        onChange: (type: string) => void;
      }
    | {
        multiple: true;
        /** The selected types. Empty means "no type filter". */
        value: string[];
        onChange: (types: string[]) => void;
      }
  );

/** What the trigger says when several — or no — types are chosen. */
function summarize(selected: string[]): { label: string; tone: string } {
  if (selected.length === 0) return { label: "any type", tone: "dim" };
  if (selected.length === 1) {
    const meta = TYPE_META.find((t) => t.value === selected[0]);
    if (meta) return { label: meta.label, tone: meta.tone };
  }
  return { label: `${selected.length} types`, tone: "accent" };
}

export function TypeSelect(props: TypeSelectProps) {
  const [open, setOpen] = useState(false);
  const close = useCallback(() => setOpen(false), []);
  const { hostRef, portal } = useAnchoredMenu(open, close, {
    height: TYPE_META.length * 34 + 42,
  });

  const multiple = props.multiple === true;
  // One array either way, so the menu below is written once. In single mode it
  // holds exactly the current type, which is also what marks that row `current`.
  const selected = multiple
    ? (props.value as string[])
    : [(props.value as string | null | undefined) ?? "task"];

  const pick = (type: string) => {
    if (props.multiple === true) {
      const next = selected.includes(type)
        ? selected.filter((t) => t !== type)
        : [...selected, type];
      props.onChange(next);
      // Deliberately NOT closed: a filter is usually more than one type, and
      // re-opening the menu between picks is the friction this replaces.
      return;
    }
    if (type !== selected[0]) props.onChange(type);
    setOpen(false);
  };

  // The trigger. In single mode it is the type's icon alone, as the modal has
  // always shown it; in multiple mode an icon carries no count, so it is
  // summarized in words.
  const single = TYPE_META.find((t) => t.value === selected[0]) ?? TYPE_META[0];
  const summary = summarize(selected);
  const tone = multiple ? summary.tone : single.tone;
  const label = multiple ? summary.label : single.label;

  const menu = portal(
    <div className="type-menu">
      <div className="type-menu-head">
        {multiple ? "Filter by work type" : "Change work type"}
      </div>
      {TYPE_META.map((t) => {
        const on = selected.includes(t.value);
        return (
          <button
            key={t.value}
            className={`type-menu-item ${t.tone}${on ? " current" : ""}`}
            // A multi-select menu is a set of independent toggles, and
            // `aria-pressed` is what says so; single-select stays a plain menu
            // item, unchanged for the modal.
            {...(multiple ? { "aria-pressed": on } : {})}
            onClick={() => pick(t.value)}
          >
            <t.Icon size={14} className="type-menu-icon" />
            <span className="type-menu-label">{t.label}</span>
          </button>
        );
      })}
    </div>,
    "type-menu-portal",
  );

  return (
    <div ref={hostRef} className="task-type-row">
      <button
        className={`type-select-trigger ${tone}`}
        aria-label={props.ariaLabel ?? `work type: ${label}`}
        title={label}
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
      >
        {multiple ? (
          <span className="type-select-summary">{label}</span>
        ) : (
          <single.Icon size={14} />
        )}
        <ChevronDown size={11} className="type-select-caret" />
      </button>
      {menu}
    </div>
  );
}
