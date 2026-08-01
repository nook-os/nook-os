// The pagination contract's React half (QOL sprint 2026-08).
//
// The server half is `nook_db::paging` + `Page<T>`/`PageQuery` in nook-types:
// every list endpoint takes `q`/`after`/`limit`/`sort`/`dir` and returns
// `{ rows, next_cursor }` with an OPAQUE cursor. This hook is the one place the
// frontend speaks that contract — search, sort and filters all live in the
// react-query key, so changing any of them restarts the walk from page one,
// and `after` is passed back verbatim because parsing it is exactly what the
// contract forbids.
//
// The UI half is `PagedPanel` (@nookos/ui), which renders a `PagedListState`;
// the two meet at that interface so the ui package needs no react-query.
import { useMemo, useState } from "react";
import { useInfiniteQuery } from "@tanstack/react-query";
import type { Page } from "@nookos/api";
import type { PagedListState } from "@nookos/ui";

/** What a fetcher receives — the wire's PageQuery, ready to spread into
 *  `params.query` of an openapi-fetch GET. */
export interface PageParams {
  q?: string;
  after?: string;
  limit?: number;
  sort?: string;
  dir?: "asc" | "desc";
}

export function usePagedList<T>({
  key,
  fetch,
  extra,
  limit = 50,
  enabled = true,
}: {
  /** Query-key base, e.g. `["operator", "tenants"]`. Search/sort/filters are
   *  appended automatically. */
  key: unknown[];
  fetch: (params: PageParams) => Promise<Page<T> | undefined | null>;
  /** Endpoint-specific filter values (a kind, a status…). Part of the key, so
   *  changing a filter restarts from page one like a new search does. The
   *  fetcher closes over the same state it passed in here. */
  extra?: Record<string, unknown>;
  limit?: number;
  enabled?: boolean;
}): PagedListState<T> {
  const [search, setSearch] = useState("");
  const [sort, setSort] = useState<{ key: string; desc: boolean } | null>(null);

  const query = useInfiniteQuery({
    queryKey: [...key, search, sort?.key ?? "", sort?.desc ?? false, extra ?? null],
    initialPageParam: undefined as string | undefined,
    queryFn: async ({ pageParam }) =>
      (await fetch({
        q: search || undefined,
        after: pageParam || undefined,
        limit,
        sort: sort?.key,
        dir: sort ? (sort.desc ? "desc" : "asc") : undefined,
      })) ?? { rows: [], next_cursor: null },
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    enabled,
  });

  const rows = useMemo(
    () => query.data?.pages.flatMap((p) => p.rows) ?? [],
    [query.data],
  );

  return {
    rows,
    loading: query.isLoading,
    filtered: search.length > 0,
    hasMore: !!query.hasNextPage,
    loadingMore: query.isFetchingNextPage,
    search,
    setSearch,
    sort,
    // none → asc → desc → none: the third click returns to the default order
    // (newest first) instead of leaving the table stuck in a sort.
    toggleSort: (sortKey: string) =>
      setSort((cur) =>
        cur?.key !== sortKey
          ? { key: sortKey, desc: false }
          : cur.desc
            ? null
            : { key: sortKey, desc: true },
      ),
    loadMore: () => void query.fetchNextPage(),
  };
}
