// A select that can show more than text.
//
// Native `<select>` cannot render an icon or a colour inside an option, so
// priority read as the word "high" with nothing to distinguish it at a glance —
// which is the entire job of a priority field on a board. This is the smallest
// custom listbox that fixes that: a button showing the current option, a popup
// listing the rest, and keyboard behaviour people already expect.
//
// Reusable on purpose — status, runtime, node and label pickers all want the
// same control.
//
// The popup escapes its container via `useAnchoredMenu` — see that module for
// why every menu in this app has to.
import React, { useCallback, useEffect, useState } from "react";
import { Check, ChevronDown } from "lucide-react";
import { useAnchoredMenu } from "./useAnchoredMenu";

export interface SelectOption<T extends string | number> {
  value: T;
  label: string;
  /** A glyph or small element shown before the label, in both places. */
  icon?: React.ReactNode;
  /** Colours the icon and the selected label. */
  color?: string;
  hint?: string;
}

const MAX_MENU_HEIGHT = 240;

export function Select<T extends string | number>({
  value,
  options,
  onChange,
  ariaLabel,
  className = "",
}: {
  value: T;
  options: SelectOption<T>[];
  onChange: (value: T) => void;
  ariaLabel?: string;
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const close = useCallback(() => setOpen(false), []);
  const { hostRef, portal } = useAnchoredMenu(open, close, {
    height: Math.min(options.length * 26 + 8, MAX_MENU_HEIGHT),
    matchWidth: true,
  });

  const current = options.find((o) => o.value === value) ?? options[0];

  // Highlight what is currently selected when the menu opens, so arrow keys
  // move from there rather than from the top.
  useEffect(() => {
    if (open) setActive(Math.max(0, options.findIndex((o) => o.value === value)));
  }, [open, options, value]);

  const commit = (v: T) => {
    onChange(v);
    setOpen(false);
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (!open) {
      if (e.key === "Enter" || e.key === " " || e.key === "ArrowDown") {
        e.preventDefault();
        setOpen(true);
      }
      return;
    }
    if (e.key === "Escape" || e.key === "Tab") {
      setOpen(false);
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      setActive((i) => (i + 1) % options.length);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActive((i) => (i - 1 + options.length) % options.length);
    } else if (e.key === "Enter") {
      e.preventDefault();
      commit(options[active].value);
    }
  };

  const menu = portal(
    options.map((o, i) => (
      <button
        key={String(o.value)}
        type="button"
        role="option"
        aria-selected={o.value === value}
        className={`sel-option${i === active ? " active" : ""}`}
        onMouseEnter={() => setActive(i)}
        onClick={() => commit(o.value)}
      >
        {o.icon && (
          <span className="sel-icon" style={{ color: o.color }}>
            {o.icon}
          </span>
        )}
        <span className="sel-option-label" style={{ color: o.color }}>
          {o.label}
        </span>
        {o.hint && <span className="faint small sel-hint">{o.hint}</span>}
        {o.value === value && <Check size={11} className="sel-check" />}
      </button>
    )),
    "sel-menu",
    { role: "listbox", onKeyDown },
  );

  return (
    <div ref={hostRef} className={`sel ${className}`}>
      <button
        type="button"
        className={`sel-trigger${open ? " open" : ""}`}
        aria-label={ariaLabel}
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
        onKeyDown={onKeyDown}
      >
        {current?.icon && (
          <span className="sel-icon" style={{ color: current.color }}>
            {current.icon}
          </span>
        )}
        <span className="sel-label" style={{ color: current?.color }}>
          {current?.label ?? "—"}
        </span>
        <ChevronDown size={12} className="sel-caret" />
      </button>
      {menu}
    </div>
  );
}
