// The desktop app's link rules, exercised as clicks rather than as function
// calls. The bug these cover did not look like a routing bug: clicking a
// notification navigated the one webview to the control plane's own origin,
// where Tauri denies every app command, so the app came up reporting itself
// unconfigured and device sign-in could never complete. Nothing about that is
// visible from `appPathFor` alone — it needed a real anchor and a real click.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const endpoint = { baseUrl: "https://nook.hein.network", token: "t" };
vi.mock("@nookos/api", () => ({ getEndpoint: () => endpoint }));

const openExternal = vi.fn();
vi.mock("./desktop", () => ({ openExternal: (url: string) => openExternal(url) }));

import { appPathFor, installLinkHandler, openAppLink, registerNavigator } from "./links";

describe("appPathFor", () => {
  const base = "https://nook.hein.network";

  it("recognises our own absolute links", () => {
    // The exact shape the control plane builds for a notification.
    expect(appPathFor(`${base}/sessions/abc-123`, base)).toBe("/sessions/abc-123");
    expect(appPathFor(`${base}/board?task=MAIN-9`, base)).toBe("/board?task=MAIN-9");
    expect(appPathFor(`${base}/`, base)).toBe("/");
  });

  it("leaves another control plane alone", () => {
    // Same application, different deployment — not somewhere we are signed in.
    expect(appPathFor("https://nook.example.com/board", base)).toBeNull();
  });

  it("leaves server paths that are not app routes alone", () => {
    // Served from our own origin, but by the control plane, not the router.
    expect(appPathFor(`${base}/docs`, base)).toBeNull();
    expect(appPathFor(`${base}/api/v1/nodes`, base)).toBeNull();
    expect(appPathFor("/docs", base)).toBeNull();
  });

  it("leaves non-http schemes alone", () => {
    expect(appPathFor("mailto:me@example.com", base)).toBeNull();
    expect(appPathFor("javascript:alert(1)", base)).toBeNull();
  });

  it("claims another host even with no endpoint configured", () => {
    // The desktop connect screen falls back to `tauri://localhost`, which no
    // http address matches — so nothing is ours until one is chosen.
    expect(appPathFor(`${base}/board`, "")).toBeNull();
  });

  it("routes same-origin links in the web build, which configures no endpoint", () => {
    // The browser app is served BY its control plane, so `baseUrl` is empty
    // and the page's own origin is the thing to compare against. Rejecting
    // this outright would leave notification clicks dead in the browser.
    expect(appPathFor(`${window.location.origin}/sessions/abc`, "")).toBe(
      "/sessions/abc",
    );
    expect(appPathFor("/board?task=MAIN-9", "")).toBe("/board?task=MAIN-9");
  });
});

describe("openAppLink", () => {
  beforeEach(() => openExternal.mockClear());

  it("routes one of our links through the router, and focuses the window", () => {
    const navigate = vi.fn((path: string) => void path);
    const focus = vi.spyOn(window, "focus").mockImplementation(() => {});
    const unregister = registerNavigator(navigate);

    openAppLink("https://nook.hein.network/sessions/abc");

    expect(navigate).toHaveBeenCalledWith("/sessions/abc");
    expect(openExternal).not.toHaveBeenCalled();
    // A notification arrives while you are looking at something else; landing
    // on the right screen in a background window is not arriving.
    expect(focus).toHaveBeenCalled();

    unregister();
    focus.mockRestore();
  });

  it("falls back to the OS browser for anything not ours", () => {
    const navigate = vi.fn((path: string) => void path);
    const unregister = registerNavigator(navigate);

    openAppLink("https://example.com/thing");

    expect(navigate).not.toHaveBeenCalled();
    expect(openExternal).toHaveBeenCalledWith("https://example.com/thing");
    unregister();
  });

  it("does nothing with an empty link", () => {
    openAppLink("");
    expect(openExternal).not.toHaveBeenCalled();
  });

  it("unregistering does not blank a navigator a later mount installed", () => {
    const first = vi.fn((p: string) => void p);
    const second = vi.fn((p: string) => void p);
    const undoFirst = registerNavigator(first);
    const undoSecond = registerNavigator(second);
    // StrictMode double-invokes effects: the old cleanup runs after the new
    // registration. If it cleared unconditionally, every notification click
    // after the first remount would silently do nothing.
    undoFirst();

    openAppLink("https://nook.hein.network/board");
    expect(second).toHaveBeenCalledWith("/board");
    undoSecond();
  });
});

describe("installLinkHandler", () => {
  // Typed via its implementation rather than a bare `vi.fn()`, so it satisfies
  // `installLinkHandler`'s parameter — and so a change to that signature fails
  // the typecheck here instead of passing a test against the wrong shape.
  const navigate = vi.fn((path: string) => void path);
  let teardown: () => void;

  beforeEach(() => {
    openExternal.mockClear();
    navigate.mockClear();
    document.body.innerHTML = "";
    teardown?.();
    teardown = installLinkHandler(navigate);
  });

  // Explicit, because this file now has TWO blocks installing document-level
  // listeners: without it, whether one leaks into the other depends on which
  // test happens to be last here (MAIN-279).
  afterEach(() => teardown?.());

  /**
   * Click an anchor the way the app renders one, and report the event.
   *
   * `on` picks what actually receives the click, which defaults to the anchor
   * but is usually something nested inside it in the real UI.
   */
  const click = (
    html: string,
    init: MouseEventInit = {},
    on = "a",
  ): MouseEvent => {
    document.body.innerHTML = html;
    const target = document.querySelector(on)!;
    const e = new MouseEvent("click", { bubbles: true, cancelable: true, ...init });
    target.dispatchEvent(e);
    return e;
  };

  it("routes a notification link in-app instead of navigating", () => {
    const e = click(`<a href="https://nook.hein.network/sessions/abc">go</a>`);
    expect(navigate).toHaveBeenCalledWith("/sessions/abc");
    expect(openExternal).not.toHaveBeenCalled();
    // The whole point: the webview must not move.
    expect(e.defaultPrevented).toBe(true);
  });

  it("routes a click on markup inside the anchor", () => {
    // Toasts wrap a title and body in the link, so the target is never the <a>.
    const e = click(
      `<a href="https://nook.hein.network/board?task=MAIN-9"><div id="t">title</div></a>`,
      {},
      "#t",
    );
    expect(navigate).toHaveBeenCalledWith("/board?task=MAIN-9");
    expect(e.defaultPrevented).toBe(true);
  });

  it("sends an external link to the OS browser, not the webview", () => {
    const e = click(`<a href="https://id.example.com/device?user_code=AB">approve</a>`);
    expect(openExternal).toHaveBeenCalledWith("https://id.example.com/device?user_code=AB");
    expect(navigate).not.toHaveBeenCalled();
    expect(e.defaultPrevented).toBe(true);
  });

  it("sends a control-plane page that is not an app route to the browser", () => {
    click(`<a href="/docs">API docs</a>`);
    expect(openExternal).toHaveBeenCalledWith("https://nook.hein.network/docs");
    expect(navigate).not.toHaveBeenCalled();
  });

  it("leaves in-page anchors and downloads alone", () => {
    expect(click(`<a href="#section">jump</a>`).defaultPrevented).toBe(false);
    expect(click(`<a href="https://x.example.com/f.zip" download>get</a>`).defaultPrevented)
      .toBe(false);
    expect(navigate).not.toHaveBeenCalled();
    expect(openExternal).not.toHaveBeenCalled();
  });

  it("leaves modified and non-primary clicks alone", () => {
    const url = `<a href="https://nook.hein.network/board">b</a>`;
    for (const mod of [{ metaKey: true }, { ctrlKey: true }, { shiftKey: true }, { button: 1 }]) {
      expect(click(url, mod).defaultPrevented).toBe(false);
    }
    expect(navigate).not.toHaveBeenCalled();
  });

  it("leaves a click React Router already handled alone", () => {
    // <Link> calls preventDefault first; re-navigating would be a second,
    // competing navigation for one click.
    document.body.innerHTML = `<a href="https://nook.hein.network/board">b</a>`;
    const a = document.querySelector("a")!;
    a.addEventListener("click", (e) => e.preventDefault());
    a.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
    expect(navigate).not.toHaveBeenCalled();
    expect(openExternal).not.toHaveBeenCalled();
  });

  it("stops intercepting once torn down", () => {
    teardown();
    expect(click(`<a href="https://nook.hein.network/board">b</a>`).defaultPrevented).toBe(false);
    expect(navigate).not.toHaveBeenCalled();
  });
});

// MAIN-279: the same interception in the web build, where the difference is
// what happens to a link that is NOT ours.
//
// The bug was not that the web build lacked a router — it was that a raw
// `<a href>` into the app reloaded the document to reach a route the router
// already had, throwing away the in-memory state on the way. Clicking a
// notification wiped the notification list you clicked it from.
//
// These drive the same DOM clicks as the desktop block above, against
// `"browser-default"`. The assertion that matters most is the NEGATIVE one:
// an external link must come out with `defaultPrevented === false`, because in
// a browser cancelling it breaks the URL bar, the new-tab gesture and the
// back button for no gain.
describe("installLinkHandler in the web build", () => {
  const navigate = vi.fn((path: string) => void path);
  let teardown: () => void;

  beforeEach(() => {
    openExternal.mockClear();
    navigate.mockClear();
    document.body.innerHTML = "";
    teardown?.();
    teardown = installLinkHandler(navigate, "browser-default");
  });

  afterEach(() => teardown?.());

  const click = (
    html: string,
    init: MouseEventInit = {},
    on = "a",
  ): MouseEvent => {
    document.body.innerHTML = html;
    const target = document.querySelector(on)!;
    const e = new MouseEvent("click", { bubbles: true, cancelable: true, ...init });
    target.dispatchEvent(e);
    return e;
  };

  it("routes an internal link client-side instead of reloading (AC-1)", () => {
    const e = click(`<a href="https://nook.hein.network/sessions/abc">go</a>`);
    expect(navigate).toHaveBeenCalledWith("/sessions/abc");
    // Cancelled, or the browser would reload the document over the top.
    expect(e.defaultPrevented).toBe(true);
    expect(openExternal).not.toHaveBeenCalled();
  });

  it("routes a notification's absolute link, keeping the app mounted (AC-2)", () => {
    const e = click(`<a href="https://nook.hein.network/board?task=MAIN-9">note</a>`);
    expect(navigate).toHaveBeenCalledWith("/board?task=MAIN-9");
    expect(e.defaultPrevented).toBe(true);
  });

  it("does not touch an external link — no preventDefault, no OS browser (AC-3)", () => {
    const e = click(`<a href="https://github.com/nook-os/nook-os/pull/1">pr</a>`);
    expect(e.defaultPrevented).toBe(false);
    expect(navigate).not.toHaveBeenCalled();
    // The desktop-only escape hatch must not fire in a browser.
    expect(openExternal).not.toHaveBeenCalled();
  });

  it("does not touch a same-origin page that is not an app route (AC-3)", () => {
    // `/docs` is the control plane's API reference, served from our own origin.
    const e = click(`<a href="https://nook.hein.network/docs">api docs</a>`);
    expect(e.defaultPrevented).toBe(false);
    expect(navigate).not.toHaveBeenCalled();
    expect(openExternal).not.toHaveBeenCalled();
  });

  it("leaves modified and non-primary clicks to the browser (AC-4)", () => {
    for (const init of [
      { metaKey: true },
      { ctrlKey: true },
      { shiftKey: true },
      { altKey: true },
      { button: 1 },
    ]) {
      const e = click(`<a href="https://nook.hein.network/board">board</a>`, init);
      expect(e.defaultPrevented).toBe(false);
      expect(navigate).not.toHaveBeenCalled();
    }
  });

  it("leaves in-page anchors and downloads alone (AC-4)", () => {
    expect(click(`<a href="#top">top</a>`).defaultPrevented).toBe(false);
    expect(
      click(`<a href="https://nook.hein.network/board" download>save</a>`).defaultPrevented,
    ).toBe(false);
    expect(navigate).not.toHaveBeenCalled();
  });

  it("leaves a click React Router already handled alone (AC-6)", () => {
    // `<Link>` cancels before this listener runs; re-navigating would be a
    // second navigation for one click.
    document.body.innerHTML = `<a href="https://nook.hein.network/board">board</a>`;
    const a = document.querySelector("a")!;
    a.addEventListener("click", (ev) => ev.preventDefault());
    a.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
    expect(navigate).not.toHaveBeenCalled();
  });

  it("routes a click on markup inside the anchor", () => {
    const e = click(
      `<a href="https://nook.hein.network/nodes"><span class="inner">nodes</span></a>`,
      {},
      ".inner",
    );
    expect(navigate).toHaveBeenCalledWith("/nodes");
    expect(e.defaultPrevented).toBe(true);
  });

  it("stops intercepting once torn down", () => {
    teardown();
    const e = click(`<a href="https://nook.hein.network/board">board</a>`);
    expect(navigate).not.toHaveBeenCalled();
    expect(e.defaultPrevented).toBe(false);
  });
});
