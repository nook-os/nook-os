// The tenant's repos — the one place the app reads the workspaces collection
// (MAIN-606).
//
// `GET /api/v1/workspaces` is paged and has no unbounded twin any more, so
// "give me every workspace" is not a request anybody can make. What replaces it
// is two different questions, answered by two different reads:
//
//   - a LIST (the table, and every picker) -> `useWorkspaces`, which is
//     `usePagedList` with this endpoint's fetcher. Search narrows server-side
//     via `q`; the rest of the rows arrive through `loadMore()`.
//   - a ROW whose id you already hold (a scope chip, a tab's badge, a card's
//     workspace name) -> `useWorkspace` / `useWorkspaceNames`, which read
//     `/workspaces/{id}`. Hunting for a known id inside a page is how a lookup
//     silently starts answering "unknown" for the fifty-first repo.
//
// Nothing here walks every page behind a caller's back: a bounded list that
// presents itself as complete is the habit this card ends.
import { useMemo } from "react";
import { useQueries, useQuery } from "@tanstack/react-query";
import { api, type WorkspaceDetail } from "@nookos/api";
import type { PagedListState } from "@nookos/ui";
import { usePagedList } from "./paging";

/** One workspace's react-query key. Shared by every reader of that row, so a
 *  rename invalidated here repaints the switcher, the chip and the badge. */
export const workspaceKey = (id: string) => ["workspace", id] as const;

/** The collection's key base. `usePagedList` appends search and sort. */
export const workspacesKey = ["workspaces"] as const;

/** One repo, read once — for an action that needs the row it already has the
 *  id of (a context menu, a poller) rather than a subscription to it. */
export async function getWorkspace(id: string): Promise<WorkspaceDetail | null> {
  return (
    (await api.GET("/api/v1/workspaces/{id}", { params: { path: { id } } })).data ?? null
  );
}

/** The tenant's repos, on the pagination contract. */
export function useWorkspaces(opts?: {
  limit?: number;
  enabled?: boolean;
}): PagedListState<WorkspaceDetail> {
  return usePagedList<WorkspaceDetail>({
    // The page SIZE is part of the key. `usePagedList` does not include it —
    // nothing needed it while every caller took the default — but the "is there
    // more than one repo?" probes below ask for two rows on the same resource,
    // and sharing a cache entry with the real list would serve one of them the
    // other's page.
    key: [...workspacesKey, opts?.limit ?? "default"],
    fetch: async (params) =>
      (await api.GET("/api/v1/workspaces", { params: { query: params } })).data,
    limit: opts?.limit,
    enabled: opts?.enabled,
  });
}

/** One repo by id — `null` while it loads, and for an id this tenant has no
 *  workspace for. */
export function useWorkspace(id: string | null | undefined): WorkspaceDetail | null {
  const { data } = useQuery({
    queryKey: workspaceKey(id ?? ""),
    queryFn: () => getWorkspace(id as string),
    enabled: !!id,
  });
  return data ?? null;
}

/**
 * Names for a set of ids that came from somewhere else — a session's
 * `workspace_id`, a card's, a tab's.
 *
 * One read per DISTINCT id, on the same key `useWorkspace` uses, so every
 * surface shares them: a board showing forty cards across three repos makes
 * three requests. That count follows the ids on screen, not the size of the
 * tenant.
 */
export function useWorkspaceNames(ids: (string | null | undefined)[]): Map<string, string> {
  // The array arrives fresh every render, so its CONTENT is the dependency.
  const joined = [...new Set(ids.filter((id): id is string => !!id))].sort().join(",");
  const distinct = useMemo(() => (joined ? joined.split(",") : []), [joined]);
  const results = useQueries({
    queries: distinct.map((id) => ({
      queryKey: workspaceKey(id),
      queryFn: () => getWorkspace(id),
    })),
  });
  const names = new Map<string, string>();
  results.forEach((r, i) => {
    if (r.data) names.set(distinct[i], r.data.name);
  });
  return names;
}

/**
 * The workspace called `name`, asked for by name.
 *
 * `q` searches name, slug and remote, so this is one request rather than a
 * scan of every page. A cloned repo is named "owner/repo", so a bare repo tail
 * matches too — the tolerance the clone poller has always had.
 */
export async function findWorkspaceByName(name: string): Promise<WorkspaceDetail | null> {
  const wanted = name.toLowerCase();
  const tail = (s: string) => s.toLowerCase().split("/").pop() ?? "";
  const rows =
    (await api.GET("/api/v1/workspaces", { params: { query: { q: name, limit: 50 } } }))
      .data?.rows ?? [];
  return (
    rows.find((w) => w.name.toLowerCase() === wanted) ??
    rows.find((w) => tail(w.name) === tail(wanted)) ??
    null
  );
}
