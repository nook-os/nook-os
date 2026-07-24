// Flip the Nth GFM task-list checkbox in a Markdown source string (MAIN-36).
//
// The Nth checkbox a reader clicks maps to the Nth `- [ ] ` / `- [x] ` marker in
// source order — the same order react-markdown renders them — so a body with
// several similar ACs toggles the right line (AC-6). Only that one marker
// changes; every other character, including non-checkbox markdown, is untouched.

// A task-list marker at the start of a list item: optional indentation, a
// bullet (`-`, `*`, `+`), whitespace, then `[ ]` / `[x]` / `[X]`. Anchored to
// line start (or string start) so `[ ]` inside prose is never mistaken for one.
const CHECKBOX = /(^|\n)([ \t]*[-*+][ \t]+)\[([ xX])\]/g;

/** Toggle the `index`-th checkbox (0-based); returns the new source. If `index`
 *  is out of range the source is returned unchanged. */
export function toggleTaskCheckbox(src: string, index: number): string {
  let seen = -1;
  return src.replace(CHECKBOX, (whole, lead, marker, state) => {
    seen += 1;
    if (seen !== index) return whole;
    const flipped = state === " " ? "x" : " ";
    return `${lead}${marker}[${flipped}]`;
  });
}

/** How many task-list checkboxes the source contains. */
export function countTaskCheckboxes(src: string): number {
  return (src.match(CHECKBOX) ?? []).length;
}
