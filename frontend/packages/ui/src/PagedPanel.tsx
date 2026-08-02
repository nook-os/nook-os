// The pagination contract's table — a Panel wired to search, sort, filters and
// cursor paging in one piece (QOL sprint 2026-08).
//
// The state comes in as a `PagedListState` (produced by the app package's
// `usePagedList`, which owns react-query and the wire format); this component
// only renders it. That split is what keeps @nookos/ui dependency-free while
// making a paginated list a one-expression affair at the call site:
//
//   <PagedPanel title="Tenants" list={tenants} columns={cols}
//               searchPlaceholder="Search slug or name…" empty="No tenants." />
//
// Sort lives on the columns: a column with `sortKey` gets a clickable header
// cycling none → asc → desc → none. The keys are the ENDPOINT's documented
// sort set — the server validates them, this component just asks.

import React from "react";
import { DataList, type DataColumn } from "./DataList";
import { SearchInput } from "./SearchInput";
import { Panel } from "./components";

/** What a paged list looks like to the UI — the meeting point between this
 *  component and the app-side hook that speaks the wire contract. */
export interface PagedListState<T> {
  rows: T[];
  /** True while the FIRST page is in flight. */
  loading: boolean;
  /** True when a search is active — picks "no results" over "empty". */
  filtered: boolean;
  hasMore: boolean;
  loadingMore: boolean;
  search: string;
  setSearch: (q: string) => void;
  sort: { key: string; desc: boolean } | null;
  toggleSort: (key: string) => void;
  loadMore: () => void;
}

export function PagedPanel<T>({
  title,
  list,
  columns,
  rowKey,
  searchPlaceholder = "Search…",
  searchLabel = "Search",
  empty = "Nothing here yet.",
  noResults = "No matches.",
  actions,
  filters,
}: {
  title: React.ReactNode;
  list: PagedListState<T>;
  columns: DataColumn<T>[];
  rowKey: (row: T) => string;
  searchPlaceholder?: string;
  searchLabel?: string;
  empty?: React.ReactNode;
  noResults?: React.ReactNode;
  /** Extra title-bar actions, rendered beside the search box. */
  actions?: React.ReactNode;
  /** Endpoint-specific filter controls (a kind select, a status toggle…).
   *  Their values belong in the hook's `extra`, which restarts paging. */
  filters?: React.ReactNode;
}) {
  return (
    <Panel
      title={title}
      actions={
        <>
          {filters}
          <SearchInput
            onSearch={list.setSearch}
            placeholder={searchPlaceholder}
            ariaLabel={typeof searchLabel === "string" ? searchLabel : "Search"}
          />
          {actions}
        </>
      }
    >
      <DataList
        columns={columns}
        rows={list.rows}
        rowKey={rowKey}
        loading={list.loading}
        filtered={list.filtered}
        empty={empty}
        noResults={noResults}
        hasMore={list.hasMore}
        onLoadMore={list.loadMore}
        loadingMore={list.loadingMore}
        sort={list.sort}
        onSort={list.toggleSort}
      />
    </Panel>
  );
}
