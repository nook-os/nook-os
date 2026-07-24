// The desktop app's link rules, exercised as clicks rather than as function
// calls. The bug these cover did not look like a routing bug: clicking a
// notification navigated the one webview to the control plane's own origin,
// where Tauri denies every app command, so the app came up reporting itself
// unconfigured and device sign-in could never complete. Nothing about that is
// visible from `appPathFor` alone — it needed a real anchor and a real click.

import { beforeEach, describe, expect, it, vi } from "vitest";

const endpoint = { baseUrl: "https://nook.hein.network", token: "t" };
vi.mock("@nookos/api", () => ({ getEndpoint: () => endpoint }));

const openExternal = vi.fn();
vi.mock("./desktop", () => ({ openExternal: (url: string) => openExternal(url) }));

import { appPathFor, installLinkHandler } from "./links";

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

  it("claims nothing when no control plane is configured", () => {
    // The connect screen: there is no "our origin" yet, so nothing is ours.
    expect(appPathFor(`${base}/board`, "")).toBeNull();
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
