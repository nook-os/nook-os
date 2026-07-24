// A dense, amber-CRT data list: column defs + a page of rows + explicit
// loading / empty / no-results states + a cursor-driven "load more".
//
// Built general on purpose (MAIN-43): the operator audit table is the first
// caller, and the tenants/nodes/bindings tables and the Settings page adopt it
// next. It knows nothing about audit rows — it takes `columns` (each with its
// own cell renderer) and `rows`, and leaves fetching, searching, and cursoring
// to the caller. It reuses the existing `.op-table` look so it drops into the
// operator page without a restyle.

import React from "react";

export interface DataColumn<T> {
  /** Stable key for React and for the header/cell pairing. */
  key: string;
  header: React.ReactNode;
  cell: (row: T) => React.ReactNode;
  /** Applied to both the `<th>` and each `<td>` in the column. */
  className?: string;
}

export type DataListPhase = "loading" | "empty" | "no-results" | "rows";

/** Which state the list is in, from whether it is loading, how many rows it
 *  holds, and whether a search filter is active. Pure and exported so the
 *  "empty log" vs "search found nothing" distinction — the one that is easy to
 *  get wrong — is testable without a DOM. Rows win as soon as there are any,
 *  even mid-refetch, so appending a page never flashes a spinner. */
export function dataListPhase(opts: {
  loading: boolean;
  count: number;
  filtered: boolean;
}): DataListPhase {
  if (opts.count > 0) return "rows";
  if (opts.loading) return "loading";
  return opts.filtered ? "no-results" : "empty";
}

export function DataList<T>({
  columns,
  rows,
  rowKey,
  loading = false,
  filtered = false,
  empty = "Nothing here yet.",
  noResults = "No matches.",
  loadingLabel = "Loading…",
  hasMore = false,
  onLoadMore,
  loadingMore = false,
}: {
  columns: DataColumn<T>[];
  rows: T[];
  rowKey: (row: T) => string;
  /** True while the FIRST page is in flight (shows the loading note). */
  loading?: boolean;
  /** True when a search/filter is active — picks `noResults` over `empty`. */
  filtered?: boolean;
  empty?: React.ReactNode;
  noResults?: React.ReactNode;
  loadingLabel?: React.ReactNode;
  /** A next cursor exists — render the "load more" control. */
  hasMore?: boolean;
  onLoadMore?: () => void;
  /** True while a "load more" fetch is in flight. */
  loadingMore?: boolean;
}) {
  const phase = dataListPhase({ loading, count: rows.length, filtered });
  return (
    <div className="data-list">
      <div className="data-list-wrap op-table-wrap">
        <table className="op-table data-list-table">
          <thead>
            <tr>
              {columns.map((c) => (
                <th key={c.key} className={c.className}>
                  {c.header}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr key={rowKey(r)}>
                {columns.map((c) => (
                  <td key={c.key} className={c.className}>
                    {c.cell(r)}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {phase === "loading" && (
        <div className="data-list-note faint">{loadingLabel}</div>
      )}
      {phase === "empty" && <div className="data-list-note empty">{empty}</div>}
      {phase === "no-results" && (
        <div className="data-list-note empty">{noResults}</div>
      )}
      {phase === "rows" && hasMore && (
        <div className="data-list-more">
          <button
            type="button"
            className="data-list-more-btn"
            onClick={onLoadMore}
            disabled={loadingMore}
          >
            {loadingMore ? "Loading…" : "Load more"}
          </button>
        </div>
      )}
    </div>
  );
}
