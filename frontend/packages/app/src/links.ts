// Where a clicked link is allowed to take the app.
//
// The packaged app has a single webview and no address bar, so a link that
// navigates it is a link that replaces the whole application — and on any
// origin but its own, Tauri denies every command the app defines.
//
// The web build needs this too, for a different reason (MAIN-279). A browser
// knows perfectly well what to do with an EXTERNAL link and should be left to
// it. But a raw `<a href>` pointing back into the app reloads the document to
// reach a route the router already has, throwing away every piece of in-memory
// state on the way — which is why clicking a notification used to wipe the
// notification list. So: same interception, and the two builds differ only in
// what they do with a link that is not ours. The visible result was an app that reported itself unconfigured
// after clicking a notification, and a device sign-in that could never
// complete, because both landed on a remote origin where `load_endpoint` and
// `device_start` come back "not allowed by ACL".
//
// Notification links are absolute on purpose — the control plane builds them
// from its public URL so the same link works in Slack or a phone push. So the
// app cannot ask for relative links; it has to recognise its own.

import { getEndpoint } from "@nookos/api";
import { openExternal } from "./desktop";

/**
 * The first path segment of every in-app route (see the router in `index.tsx`).
 *
 * An allowlist, not a guess: the control plane serves paths that are NOT app
 * routes from the very same origin — `/docs` is its API reference, `/api/…` is
 * the API itself. Treating "same host" as "same app" would swallow those into
 * a router that has no such route and render Nothing here.
 */
const APP_ROUTES = new Set([
  "",
  "workspaces",
  "sessions",
  "board",
  "operator",
  "activity",
  "nodes",
  "settings",
  "feedback",
  "help",
]);

/**
 * The in-app route this href refers to, or `null` if it belongs elsewhere.
 *
 * Pure, and exported for that reason: every judgement about whether a link
 * stays inside the app is made here, where it can be reasoned about, rather
 * than inside a DOM event handler.
 */
export function appPathFor(href: string, baseUrl: string): string | null {
  let url: URL;
  let base: URL;
  try {
    // The configured control plane in the packaged app; the page's own origin
    // in a browser, where the app IS served by the control plane. On the
    // desktop connect screen neither applies — the base is `tauri://localhost`,
    // which no http link can match, so nothing is claimed. Which is right:
    // until an endpoint is chosen, no address is ours.
    base = new URL(baseUrl || window.location.origin);
    url = new URL(href, base);
  } catch {
    return null;
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") return null;
  // Another control plane's URL is somebody else's app, even though it runs
  // the same code. We are signed in to this one.
  if (url.host !== base.host) return null;
  if (!APP_ROUTES.has(url.pathname.split("/")[1] ?? "")) return null;
  return url.pathname + url.search + url.hash;
}

/**
 * How to navigate, once something outside React needs to.
 *
 * A desktop notification is fired from a plain module and clicked minutes
 * later, possibly while the app is in the background — there is no component
 * in scope and no hook to call. Registering the router's `navigate` once gives
 * that click somewhere to go without every caller reaching for the router.
 */
let navigateTo: ((path: string) => void) | null = null;

export function registerNavigator(fn: (path: string) => void): () => void {
  navigateTo = fn;
  return () => {
    // Only clear our own, so an unmount racing a remount cannot blank the
    // navigator the new mount just set.
    if (navigateTo === fn) navigateTo = null;
  };
}

/**
 * Follow one of our own links from outside the DOM — a notification click.
 *
 * Focuses the window first: the whole point of a desktop notification is that
 * it reaches you when you are looking at something else, so arriving at the
 * right screen in a window still behind your editor would be no arrival at all.
 */
export function openAppLink(href: string): void {
  if (!href) return;
  try {
    window.focus();
  } catch {
    // Focus is a courtesy; navigation is the job.
  }
  const path = appPathFor(href, getEndpoint().baseUrl);
  if (path && navigateTo) navigateTo(path);
  else void openExternal(absolute(href));
}

/**
 * Best-effort absolute form of an href, for handing to the OS browser.
 *
 * A relative link before any control plane is configured — the connect screen —
 * has no meaningful base: `tauri://localhost` serves the app bundle, not a
 * server. Passing the href through unresolved lets the OS reject something
 * unopenable, which is better than throwing inside a click handler and leaving
 * the link silently dead.
 */
function absolute(href: string): string {
  const { baseUrl } = getEndpoint();
  if (!baseUrl) return href;
  try {
    return new URL(href, baseUrl).toString();
  } catch {
    return href;
  }
}

/** Should this click be ours to handle, or does it already mean something? */
function isPlainClick(e: MouseEvent): boolean {
  return (
    !e.defaultPrevented &&
    e.button === 0 &&
    !e.metaKey &&
    !e.ctrlKey &&
    !e.shiftKey &&
    !e.altKey
  );
}

/**
 * What a click on a link that is NOT one of ours should do.
 *
 * The only thing the two builds disagree about (MAIN-279). Internal links
 * behave identically in both — cancel, and route.
 */
export type ExternalLinks =
  /**
   * Hand it to the OS browser, cancelling the click. The packaged app has one
   * webview and no address bar, so a link that navigates it replaces the whole
   * application with a page where Tauri denies every command the app defines.
   */
  | "os-browser"
  /**
   * Leave the click completely alone. In a browser an external link is just a
   * link: there is a URL bar, a back button, and a tab to open it in, and
   * cancelling the default would break every one of those for no gain.
   */
  | "browser-default";

/**
 * Intercept link clicks. Returns a teardown.
 *
 * One document-level listener rather than a fixed-up `<a>` in each of the dozen
 * places that render one — including markdown, where the hrefs come from
 * whatever an agent wrote and no call site exists to fix. A rule enforced in one
 * place cannot be forgotten by the next component that renders a link.
 *
 * **Both builds run this** (MAIN-279). It was desktop-only, on the reasoning
 * that "the web build needs none of this: a link is a link". True of external
 * links, and wrong about our own: a raw `<a href>` pointing INTO the app still
 * reloads the whole document, which throws away every piece of in-memory state
 * the app holds. The visible case was notifications — clicking one wiped the
 * list you clicked it from — but it costs a full remount on any raw anchor.
 * React Router's `<Link>` was never the problem; the anchors that are not one
 * are, and there is no call site to fix for markdown.
 *
 * React Router's own `<Link>` calls `preventDefault` before this listener runs,
 * so its navigations pass straight through untouched.
 */
export function installLinkHandler(
  navigate: (path: string) => void,
  external: ExternalLinks = "os-browser",
): () => void {
  const onClick = (e: MouseEvent) => {
    if (!isPlainClick(e)) return;
    const anchor = (e.target as Element | null)?.closest?.("a");
    if (!anchor) return;

    const href = anchor.getAttribute("href");
    // In-page anchors, and downloads, already do the right thing.
    if (!href || href.startsWith("#") || anchor.hasAttribute("download")) return;

    const path = appPathFor(href, getEndpoint().baseUrl);
    if (path) {
      // Ours, in both builds: the document must not reload to reach a route the
      // router already has.
      e.preventDefault();
      navigate(path);
      return;
    }
    // Not ours. On the web that means hands off entirely — no preventDefault,
    // so the browser opens it exactly as it would have without this listener.
    if (external === "browser-default") return;
    e.preventDefault();
    void openExternal(absolute(href));
  };

  document.addEventListener("click", onClick);
  return () => document.removeEventListener("click", onClick);
}
