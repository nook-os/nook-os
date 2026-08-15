// The browser tab's title (MAIN-450).
//
// Three tabs all reading "NookOS" is three tabs you have to click through to
// find the one you wanted. The title names the SECTION you are in, and nothing
// finer: `/sessions/:id` is still "Sessions", because a tab strip truncates
// long titles and the section is the part that distinguishes the tabs.
import { useEffect, useSyncExternalStore } from "react";
import { matchPath, useLocation } from "react-router-dom";
import { useQueryClient, type QueryClient } from "@tanstack/react-query";

import { PENDING_KEY } from "./Interactions";
import { NOTIFICATIONS_KEY } from "./Notifications";

/** What the app calls itself everywhere it speaks to a person. */
export const BRAND = "nook@os";

/** U+00B7 with a space each side, between section and brand. */
const SEPARATOR = " · ";

/** U+25CF and one space. Presence, not a count (NG-2). */
const ATTENTION = "● ";

/// Route pattern → the name the nav already gives that place.
///
/// The strings are `SECTIONS` / `ADMIN_SECTION` and the top-bar links in
/// `layout.tsx`, and the patterns are the route table's in `index.tsx`. Both
/// halves are copied rather than derived because the two tables have different
/// shapes — but neither may be re-worded here: a tab that invents a second name
/// for a place the UI already named is worse than no title at all.
///
/// Matched with `matchPath`, so `/board/nonsense` is NOT Board. An unrouted path
/// is a 404, and titling it after the section it merely resembles would say the
/// tab is somewhere it is not.
const SECTIONS: ReadonlyArray<readonly [string, string]> = [
  ["/mission", "Mission Control"],
  ["/workspaces", "Workspaces"],
  ["/workspaces/:id", "Workspaces"],
  ["/sessions", "Sessions"],
  ["/sessions/list", "Sessions"],
  ["/sessions/:id", "Sessions"],
  ["/board", "Board"],
  ["/loop/:taskId", "Loop"],
  ["/chat", "Chat"],
  ["/nodes", "Nodes"],
  ["/nodes/:id", "Nodes"],
  ["/notebook", "Notes"],
  ["/admin", "Admin"],
  ["/operator", "Admin"],
  ["/settings", "Settings"],
  ["/team", "Team"],
  ["/help", "Docs"],
  ["/verify-email", "Verify email"],
  ["/accept", "Accept invite"],
];

/// The signed-out app is two screens: the invite landing, which renders without
/// auth so an invitee can see who invited them, and the login catch-all.
function signedOutSection(pathname: string): string {
  return matchPath("/accept", pathname) ? "Accept invite" : "Sign in";
}

function signedInSection(pathname: string): string | null {
  for (const [pattern, name] of SECTIONS) {
    if (matchPath(pattern, pathname)) return name;
  }
  // The dashboard at `/`, and anything unrouted. Both are the bare brand: there
  // is no section to name, and the alternative — leaving the previous route's
  // title up — makes the tab lie about where it is.
  return null;
}

/**
 * The whole title, from the three things it depends on. Pure, so every row of
 * the map is a unit test rather than a render.
 */
export function resolveTitle(
  pathname: string,
  signedIn: boolean,
  hasAttention: boolean,
): string {
  const section = signedIn ? signedInSection(pathname) : signedOutSection(pathname);
  const base = section ? `${section}${SEPARATOR}${BRAND}` : BRAND;
  return hasAttention ? `${ATTENTION}${base}` : base;
}

/// Is anything waiting on the person? Read from the caches the shell already
/// fills, never fetched here (AC-7).
///
/// `undefined` — the shell has not mounted, or we are signed out and it never
/// will — is "nothing waiting", which is the honest answer: an unanswered
/// question we have not heard about yet is not one to dot the tab for.
function attentionFrom(qc: QueryClient): boolean {
  const pending = qc.getQueryData<unknown[]>(PENDING_KEY);
  const notifications = qc.getQueryData<{ unread?: number }>(NOTIFICATIONS_KEY);
  return (pending?.length ?? 0) > 0 || (notifications?.unread ?? 0) > 0;
}

/// Signed in, signed out, or not yet known. The third is a real state — the
/// desktop's connect screen, and the moment before `/auth/me` answers — and it
/// is why this is not a boolean: titling an unresolved app "Sign in" flashes the
/// wrong word at somebody who is signed in and merely reloading.
type Auth = "in" | "out" | "unknown";

function authFrom(qc: QueryClient): Auth {
  const me = qc.getQueryData(["me"]);
  if (me === undefined) return "unknown";
  return me === null ? "out" : "in";
}

/// Subscribe to the query cache and read one PRIMITIVE out of it.
///
/// Primitive because `useSyncExternalStore` compares snapshots by identity: a
/// selector returning a fresh object or tuple would re-render forever. Two
/// separate calls, each returning one scalar, rather than one call returning a
/// pair.
function useCacheValue<T extends string | number | boolean>(
  select: (qc: QueryClient) => T,
): T {
  const qc = useQueryClient();
  return useSyncExternalStore(
    (onChange) => qc.getQueryCache().subscribe(onChange),
    () => select(qc),
  );
}

/**
 * Keeps `document.title` in step with the route and the attention state.
 * Renders nothing.
 *
 * Mounted ONCE, at the top of the app inside the router, rather than inside the
 * auth gate: that component has six early returns — starting, connect, local
 * stack failed, connecting, signed out, signed in — and a title mounted in each
 * is five places for the next one to be forgotten. Reading auth from the `["me"]`
 * cache gets the same answer from outside the branching.
 */
export function DocumentTitle() {
  const { pathname } = useLocation();
  const auth = useCacheValue(authFrom);
  const attention = useCacheValue(attentionFrom);

  useEffect(() => {
    // Nothing is known yet, so leave `index.html`'s title alone — it is already
    // the bare brand, which is exactly what this would set. Writing "Sign in"
    // here instead would flash it at every signed-in reload.
    if (auth === "unknown") return;
    document.title = resolveTitle(pathname, auth === "in", attention);
  }, [pathname, auth, attention]);

  return null;
}
