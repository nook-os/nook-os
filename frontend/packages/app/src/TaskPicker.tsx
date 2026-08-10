import React, { useEffect, useMemo, useRef, useState } from "react";
import { api } from "@nookos/api";

/** One search result, reduced to what a picker row needs to render. */
export type PickerTask = {
  id: string;
  key: string | null;
  title: string;
  /** The column's TYPE, not its name — `completed` is what "done" means. */
  column_type?: string | null;
  type?: string | null;
};

export type TaskPickerProps = {
  /** Placeholder for the search box; the caller names what it is picking. */
  placeholder?: string;
  /**
   * Issue types to search. **Pass them explicitly.** The server excludes
   * `epic` unless the filter names it (MAIN-80), so a picker that omits this
   * silently cannot find epics — the lesson MAIN-181 learned the hard way.
   */
  types?: string[];
  /** Restrict to one board, by id or key. Omitted searches the tenant. */
  board?: string;
  /** How many results to show. Ten unless a caller has a reason. */
  limit?: number;
  /** Debounce for the server search, in ms. */
  debounceMs?: number;
  /**
   * Task ids that may not be chosen, each with the reason to show in place of
   * a click. Keeping the REASON here rather than a bare id list is what lets a
   * row say "already linked" instead of just looking broken.
   */
  disabledIds?: Record<string, string>;
  /** Tag results whose column type is `completed`/`canceled`, and say so. */
  doneNote?: string;
  /** Chosen. The picker does not clear itself — the caller decides. */
  onPick: (task: PickerTask) => void;
  /** Dismissed without picking (Escape, or a click outside). */
  onCancel?: () => void;
  autoFocus?: boolean;
};

/** Done = the work is over, either way it ended. */
export function isDone(t: PickerTask): boolean {
  return t.column_type === "completed" || t.column_type === "canceled";
}

/**
 * A debounced, server-backed task search dropdown (MAIN-194 AC-8).
 *
 * Reusable on purpose, and the test for that is the absence of anything here
 * about dependencies: the debounce, the top-N cap and the query composition
 * live INSIDE, and everything a caller varies — which types to search, what to
 * exclude and why, whether Done is worth flagging — arrives as props. The
 * Dependencies section is the first consumer; the epic-parent picker, the board
 * epic filter and the bulk toolbar are meant to drop it in unchanged.
 *
 * Archived tickets are never searched. That is not a prop, because a picker
 * that offers to link a ticket somebody archived is offering a mistake.
 */
export function TaskPicker({
  placeholder = "search tickets…",
  types = ["task", "bug", "story", "chore", "epic"],
  board,
  limit = 10,
  debounceMs = 250,
  disabledIds = {},
  doneNote,
  onPick,
  onCancel,
  autoFocus = true,
}: TaskPickerProps) {
  const [term, setTerm] = useState("");
  const [debounced, setDebounced] = useState("");
  const [results, setResults] = useState<PickerTask[]>([]);
  const [searching, setSearching] = useState(false);
  const boxRef = useRef<HTMLDivElement>(null);

  // The debounce. A keystroke-per-request search is a request storm against a
  // query that scans titles; 250ms is the pause that reads as "I stopped
  // typing" without feeling laggy.
  useEffect(() => {
    const t = setTimeout(() => setDebounced(term.trim()), debounceMs);
    return () => clearTimeout(t);
  }, [term, debounceMs]);

  // `types` is an array prop, so a caller passing a literal would re-run this
  // effect on every render. Freeze it to its contents.
  const typeKey = types.join(",");

  useEffect(() => {
    if (!debounced) {
      setResults([]);
      return;
    }
    let cancelled = false;
    setSearching(true);
    (async () => {
      const res = await api.GET("/api/v1/tasks", {
        params: {
          query: {
            q: debounced,
            // Explicit, so epics are included rather than silently dropped.
            type: typeKey.split(","),
            // Never offer to link something somebody archived.
            archived: false,
            // Finished cards ARE findable here (MAIN-464): the pick query hides
            // Done and Canceled because they are not work, but linking a
            // dependency to a ticket that already shipped is ordinary — which
            // is what `doneNote` exists to label.
            done: true,
            limit,
            ...(board ? { board } : {}),
          },
        },
      });
      if (cancelled) return;
      setSearching(false);
      const rows = (res.data ?? []) as PickerTask[];
      setResults(rows.slice(0, limit));
    })();
    return () => {
      cancelled = true;
    };
  }, [debounced, typeKey, board, limit]);

  // Escape cancels; a click outside cancels. Both because a dropdown you
  // cannot dismiss is a modal nobody asked for.
  useEffect(() => {
    if (!onCancel) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onCancel();
    };
    const onDown = (e: MouseEvent) => {
      if (boxRef.current && !boxRef.current.contains(e.target as Node)) onCancel();
    };
    document.addEventListener("keydown", onKey);
    document.addEventListener("mousedown", onDown);
    return () => {
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("mousedown", onDown);
    };
  }, [onCancel]);

  const rows = useMemo(
    () =>
      results.map((t) => ({
        task: t,
        disabledReason: disabledIds[t.id],
        done: isDone(t),
      })),
    [results, disabledIds],
  );

  return (
    <div className="task-picker" ref={boxRef}>
      <input
        className="input task-picker-input"
        placeholder={placeholder}
        value={term}
        autoFocus={autoFocus}
        onChange={(e) => setTerm(e.target.value)}
        aria-label={placeholder}
      />
      {debounced && (
        <div className="task-picker-results" role="listbox">
          {searching && <div className="task-picker-empty faint small">searching…</div>}
          {!searching && rows.length === 0 && (
            <div className="task-picker-empty faint small">no matches</div>
          )}
          {rows.map(({ task, disabledReason, done }) => (
            <button
              key={task.id}
              type="button"
              role="option"
              aria-selected={false}
              aria-disabled={!!disabledReason}
              disabled={!!disabledReason}
              className={`task-picker-row${disabledReason ? " disabled" : ""}`}
              title={disabledReason ?? task.title}
              onClick={() => !disabledReason && onPick(task)}
            >
              <span className="mono task-picker-key">{task.key ?? "—"}</span>
              <span className="task-picker-title">{task.title}</span>
              {done && <span className="task-picker-done">Done</span>}
              {disabledReason && (
                <span className="faint small task-picker-why">{disabledReason}</span>
              )}
            </button>
          ))}
          {/* Said once, under the list, rather than on every Done row: the
              point is that picking one is allowed and simply will not gate
              anything — a per-row essay would bury the results. */}
          {doneNote && rows.some((r) => r.done) && (
            <div className="task-picker-note faint small">{doneNote}</div>
          )}
        </div>
      )}
    </div>
  );
}
