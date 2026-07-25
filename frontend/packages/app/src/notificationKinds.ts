// The kinds↔checkbox mapping for a channel's notification filter (MAIN-92).
//
// A channel stores a `kinds` array the backend prefix-matches: `[]` means
// everything, `"task."` catches every task kind, `"task.claimed"` catches one.
// The settings checklist is a *view* of that array — these pure functions turn
// the stored array into checkbox state and back into the MINIMAL array that
// reproduces it, so the UI never has to think in prefix strings.

import type { NotificationKind } from "@nookos/api";

/** One row-group in the checklist: a dotted prefix and the kinds under it. */
export interface KindGroup {
  prefix: string; // "task."
  label: string; // "Tasks"
  kinds: NotificationKind[];
}

/** The checklist's state, derived from (and re-encoded to) a `kinds` array. */
export interface KindsState {
  /** `kinds === []` — deliver everything. Distinct from all-boxes-checked. */
  everything: boolean;
  /** The kind ids currently ticked (a whole group ticks all of its kinds). */
  checked: Set<string>;
  /** Stored filters that match no catalogued kind — shown as raw chips. */
  chips: string[];
}

const GROUP_LABELS: Record<string, string> = {
  "task.": "Tasks",
  "node.": "Nodes",
  "session.": "Sessions",
  "git.": "Git",
  "skill.": "Skills",
};

function capitalize(s: string): string {
  return s ? s[0].toUpperCase() + s.slice(1) : s;
}

/** A readable name for a group prefix — a known label, else a capitalised stem. */
export function groupLabel(prefix: string): string {
  return GROUP_LABELS[prefix] ?? capitalize(prefix.replace(/\.$/, ""));
}

/** Group the catalog by its `group` prefix, preserving first-seen order. */
export function groupsOf(catalog: NotificationKind[]): KindGroup[] {
  const order: string[] = [];
  const by = new Map<string, NotificationKind[]>();
  for (const k of catalog) {
    if (!by.has(k.group)) {
      by.set(k.group, []);
      order.push(k.group);
    }
    by.get(k.group)!.push(k);
  }
  return order.map((prefix) => ({
    prefix,
    label: groupLabel(prefix),
    kinds: by.get(prefix)!,
  }));
}

/** Whether a stored filter `f` is a prefix of at least one catalogued kind. */
function knownFilter(f: string, catalog: NotificationKind[]): boolean {
  return catalog.some((k) => k.id.startsWith(f));
}

/**
 * Turn a stored `kinds` array into checkbox state.
 *
 * `[]` is the explicit "everything" state. Otherwise a kind is checked when any
 * stored filter is a prefix of its id (mirroring the backend's `starts_with`),
 * and any filter that is a prefix of nothing catalogued survives as a chip so it
 * is never silently dropped (AC-4).
 */
export function decode(kinds: string[], catalog: NotificationKind[]): KindsState {
  if (kinds.length === 0) {
    return { everything: true, checked: new Set(), chips: [] };
  }
  const checked = new Set<string>();
  for (const k of catalog) {
    if (kinds.some((f) => k.id.startsWith(f))) checked.add(k.id);
  }
  const chips = kinds.filter((f) => !knownFilter(f, catalog));
  return { everything: false, checked, chips };
}

/**
 * Turn checkbox state back into the MINIMAL `kinds` array (AC-3).
 *
 * "Everything" → `[]`. Otherwise a fully-ticked group collapses to its single
 * prefix, a partially-ticked group emits the ticked kind ids, and chips ride
 * along verbatim. Encoding then decoding is a fixed point, so the checklist
 * round-trips across a save/reload.
 */
export function encode(state: KindsState, catalog: NotificationKind[]): string[] {
  if (state.everything) return [];
  const out: string[] = [];
  for (const g of groupsOf(catalog)) {
    const ticked = g.kinds.filter((k) => state.checked.has(k.id));
    if (ticked.length === g.kinds.length && g.kinds.length > 0) {
      out.push(g.prefix); // whole group → one prefix
    } else {
      for (const k of ticked) out.push(k.id);
    }
  }
  out.push(...state.chips);
  return out;
}
