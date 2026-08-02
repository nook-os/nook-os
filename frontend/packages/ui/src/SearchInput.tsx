// A debounced search box. Type freely; `onSearch` fires once the typing pauses,
// so a server-side search (MAIN-43) runs per burst, not per keystroke.
//
// The input is controlled internally (so the field stays responsive) while the
// debounced value is what escapes to the caller. Reusable — audit today, the
// other operator tables and Settings next.

import React, { useMemo, useState } from "react";
import { Search, X } from "lucide-react";
import { debounce } from "./debounce";

export function SearchInput({
  onSearch,
  placeholder = "Search…",
  delay = 300,
  initial = "",
  ariaLabel = "Search",
  iconTitle,
}: {
  onSearch: (q: string) => void;
  placeholder?: string;
  /** Debounce window in ms. */
  delay?: number;
  initial?: string;
  ariaLabel?: string;
  /** Hover tooltip on the magnifier icon (MAIN-176 AC-2). Opt-in so callers that
   *  don't want it (e.g. Settings) are unchanged. */
  iconTitle?: string;
}) {
  const [value, setValue] = useState(initial);

  // One debounced emitter, rebuilt only if the callback or delay changes — a
  // fresh one per render would never accumulate calls to actually debounce.
  const emit = useMemo(() => debounce(onSearch, delay), [onSearch, delay]);

  return (
    <span className="search-input">
      {/* The tooltip lives on a wrapping span — lucide icons don't take `title`
          (MAIN-176 AC-2). The magnifier stays decorative (aria-hidden). */}
      <span className="search-input-icon" title={iconTitle} aria-hidden="true">
        <Search size={13} />
      </span>
      <input
        className="search-input-field"
        type="search"
        value={value}
        placeholder={placeholder}
        aria-label={ariaLabel}
        onChange={(e) => {
          setValue(e.target.value);
          emit(e.target.value);
        }}
      />
      {/* Our own clear control — the native WebKit one is Chrome-blue and is
          suppressed in CSS. Clearing emits immediately: an empty box that
          still shows filtered rows reads as broken. */}
      {value && (
        <button
          type="button"
          className="search-input-clear"
          title="clear"
          aria-label="clear search"
          onClick={() => {
            setValue("");
            onSearch("");
          }}
        >
          <X size={12} />
        </button>
      )}
    </span>
  );
}
