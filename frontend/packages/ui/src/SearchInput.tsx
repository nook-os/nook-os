// A debounced search box. Type freely; `onSearch` fires once the typing pauses,
// so a server-side search (MAIN-43) runs per burst, not per keystroke.
//
// The input is controlled internally (so the field stays responsive) while the
// debounced value is what escapes to the caller. Reusable — audit today, the
// other operator tables and Settings next.

import React, { useMemo, useState } from "react";
import { Search } from "lucide-react";
import { debounce } from "./debounce";

export function SearchInput({
  onSearch,
  placeholder = "Search…",
  delay = 300,
  initial = "",
  ariaLabel = "Search",
}: {
  onSearch: (q: string) => void;
  placeholder?: string;
  /** Debounce window in ms. */
  delay?: number;
  initial?: string;
  ariaLabel?: string;
}) {
  const [value, setValue] = useState(initial);

  // One debounced emitter, rebuilt only if the callback or delay changes — a
  // fresh one per render would never accumulate calls to actually debounce.
  const emit = useMemo(() => debounce(onSearch, delay), [onSearch, delay]);

  return (
    <span className="search-input">
      <Search size={13} className="search-input-icon" aria-hidden="true" />
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
    </span>
  );
}
