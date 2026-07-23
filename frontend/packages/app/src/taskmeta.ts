// Priority presentation, in one place.
//
// The numbers follow Linear's convention so values port cleanly (0 none,
// 1 urgent … 4 low), which has one consequence worth stating loudly wherever
// priority is rendered or sorted: **0 is not the lowest priority, it is the
// absence of one.** "Nobody has triaged this" and "this is the least important
// thing on the board" are different claims, and a board that sorted 0 first
// would put every untriaged task above the urgent ones.

export interface PriorityMeta {
  value: number;
  label: string;
  /** Single glyph for a dense card. */
  mark: string;
  color: string;
}

export const PRIORITIES: PriorityMeta[] = [
  { value: 0, label: "none", mark: "·", color: "var(--nook-fg-faint)" },
  { value: 1, label: "urgent", mark: "!!", color: "#f14668" },
  { value: 2, label: "high", mark: "↑", color: "#f5a524" },
  { value: 3, label: "medium", mark: "=", color: "var(--nook-accent)" },
  { value: 4, label: "low", mark: "↓", color: "var(--nook-fg-faint)" },
];

export function priorityMeta(value: number | null | undefined): PriorityMeta {
  return PRIORITIES.find((p) => p.value === (value ?? 0)) ?? PRIORITIES[0];
}

/** The board's sort: urgent first, unset last, then oldest — matching the API. */
export function priorityRank(value: number | null | undefined): number {
  const v = value ?? 0;
  return v === 0 ? 5 : v;
}

/**
 * Markdown reduced to a one-line preview for a card.
 *
 * A task body is a spec: frontmatter, headings, checklists, code. Rendering it
 * raw on a card produced literal `## Acceptance - [ ] **AC-1** …`, which is
 * both ugly and useless at that size — the card should say what the task is,
 * and the detail panel is where the spec is read.
 */
export function previewText(md: string | null | undefined): string {
  if (!md) return "";
  let out = md;
  // Frontmatter is metadata for a machine, never a summary for a person.
  out = out.replace(/^---\n[\s\S]*?\n---\n?/, "");
  out = out
    .split("\n")
    // Fences and rules carry no words.
    .filter((l) => !/^\s*(```|---|===)/.test(l))
    .map((l) =>
      l
        .replace(/^\s*#{1,6}\s*/, "")
        .replace(/^\s*[-*]\s+\[[ xX]\]\s*/, "")
        .replace(/^\s*[-*]\s+/, ""),
    )
    .join(" ");
  // Emphasis and code ticks: keep the text, drop the punctuation.
  out = out.replace(/\*\*([^*]+)\*\*/g, "$1").replace(/`([^`]+)`/g, "$1");
  return out.replace(/\s+/g, " ").trim();
}
