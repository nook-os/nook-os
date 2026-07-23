// Where the API lives, and what proves who we are.
//
// In a browser these are both answered by the page itself: requests go to the
// same origin, and the session cookie rides along. A packaged desktop app has
// neither — it is served from `tauri://localhost`, which is not a control
// plane and never will be — so it has to be told, and it has to carry a
// credential a custom scheme can actually send.
//
// Nothing here is browser-specific: with no endpoint configured, every value
// below collapses to the same-origin behaviour the web app already had.

export interface Endpoint {
  /** e.g. `https://nook.example.com`. Empty means same-origin. */
  baseUrl: string;
  /** A `nook_user_…` token, sent as a bearer. Empty means cookie auth. */
  token: string;
}

let current: Endpoint = { baseUrl: "", token: "" };

/** Point the client at a control plane. Called once at startup. */
export function setEndpoint(next: Partial<Endpoint>): void {
  current = {
    // Trailing slashes turn `/api/v1/x` into `//api/v1/x`, which some proxies
    // treat as a different path entirely.
    baseUrl: (next.baseUrl ?? current.baseUrl).replace(/\/+$/, ""),
    token: next.token ?? current.token,
  };
}

export function getEndpoint(): Endpoint {
  return current;
}

/** True when we are talking to a control plane that is not this origin. */
export function isRemote(): boolean {
  return current.baseUrl !== "";
}

/** Absolute URL for an API path. */
export function apiUrl(path: string): string {
  return current.baseUrl ? `${current.baseUrl}${path}` : path;
}

/** ws:// or wss:// URL for a socket path. */
export function socketUrl(path: string): string {
  if (current.baseUrl) {
    // Reuse the endpoint's own scheme rather than the page's: a desktop app is
    // served over `tauri://`, which says nothing about whether the control
    // plane is https.
    return current.baseUrl.replace(/^http/, "ws") + path;
  }
  const proto = window.location.protocol === "https:" ? "wss" : "ws";
  return `${proto}://${window.location.host}${path}`;
}

/**
 * Open an authenticated WebSocket against the endpoint.
 *
 * The one place a socket is constructed, on purpose. The token rides in the
 * subprotocol, and forgetting it is invisible in a browser — a same-origin
 * socket authenticates by cookie — but silently anonymous from the desktop app,
 * which sends no cookie. Two call sites once built their own sockets and
 * omitted it, so the desktop app's live feed and terminals never connected
 * while REST worked. Routing every socket through here makes that drift
 * impossible rather than merely fixed.
 */
export function openSocket(path: string): WebSocket {
  return new WebSocket(socketUrl(path), socketProtocols());
}

/**
 * The subprotocol pair a WebSocket uses to authenticate.
 *
 * A browser WebSocket cannot set an Authorization header and, cross-origin,
 * sends no cookie — so the token travels in the one field a client controls.
 * Undefined when no token is configured, which is the same-origin case where
 * the cookie already works.
 */
export function socketProtocols(): string[] | undefined {
  return current.token ? ["nook.bearer", current.token] : undefined;
}

/** Headers that authenticate a request, when a token is configured. */
export function authHeaders(): Record<string, string> {
  return current.token ? { Authorization: `Bearer ${current.token}` } : {};
}
