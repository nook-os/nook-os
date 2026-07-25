// Thin, fully-typed API client. All types are generated from the Rust
// OpenAPI document — regenerate with `./scripts/gen-types.sh`.
import createClient from "openapi-fetch";
import type { paths, components } from "./generated/schema";
import { apiUrl, authHeaders, isRemote, openSocket } from "./endpoint";
import { reportWriteFailure } from "./write-failure";

export type Schemas = components["schemas"];
export type Tenant = Schemas["Tenant"];
export type User = Schemas["User"];
export type MeResponse = Schemas["MeResponse"];
export type TenantMembership = Schemas["TenantMembership"];
export type Capabilities = Schemas["Capabilities"];
export type NodeInfo = Schemas["Node"];
export type Workspace = Schemas["Workspace"];
export type WorkspaceLocation = Schemas["WorkspaceLocation"];
export type Session = Schemas["Session"];
export type Board = Schemas["Board"];
export type BoardColumn = Schemas["BoardColumn"];
export type TaskItem = Schemas["TaskItem"];
export type Notification = Schemas["Notification"];
export type NotificationChannel = Schemas["NotificationChannel"];
export type ChannelKind = Schemas["ChannelKind"];
export type Invite = Schemas["Invite"];
export type TaskDetail = Schemas["TaskDetail"];
export type TaskLabel = Schemas["Label"];
export type TaskComment = Schemas["TaskComment"];
export type RelatedTask = Schemas["RelatedTask"];
export type EventItem = Schemas["Event"];
export type Note = Schemas["Note"];
export type Theme = Schemas["Theme"];
export type DispatchSuggestion = Schemas["DispatchSuggestion"];
export type OperatorAuditEntry = Schemas["OperatorAuditEntry"];
export type OperatorAuditPage = Schemas["OperatorAuditPage"];
export type OperatorTenant = Schemas["OperatorTenant"];
export type OperatorNode = Schemas["OperatorNode"];
export type BindingRow = Schemas["BindingRow"];
// Team chat (MAIN-49/50). The chat service is a separate origin behind the
// `/chat` proxy, so its calls go through `./chat`, but its types are Rust-owned
// here like everything else.
export type ChatChannel = Schemas["ChatChannel"];
export type ChatMessage = Schemas["ChatMessage"];
export type ChatMessagePage = Schemas["ChatMessagePage"];
export type ChatServerMessage = Schemas["ChatServerMessage"];
export type UserToken = Schemas["UserToken"];
export type VaultPasskey = Schemas["VaultPasskey"];
export type TenantMemberItem = Schemas["TenantMemberItem"];

export type { paths };
export * from "./ws";
export * from "./endpoint";
export * from "./write-failure";
export * from "./chat";

// Same-origin by default: dev (Vite proxies /api) and production (the control
// plane fronts the app) both work with no configuration.
export const api = createClient<paths>({
  baseUrl: "/",
  // "same-origin", not "include". The web build is served by the control plane,
  // so cookies still ride along there. The desktop build is cross-origin and
  // authenticates with a bearer token — and a cross-origin request made with
  // `include` requires `Access-Control-Allow-Credentials` on the response or
  // the browser discards it entirely. We deliberately do not send that header,
  // because the desktop client is not meant to use the cookie session, so
  // `include` meant every desktop request failed after the connect screen had
  // already reported success.
  credentials: "same-origin",
});

// A desktop build is served from `tauri://localhost` and has no control plane
// on its own origin, so it configures an endpoint at startup. Rewriting here
// rather than at client construction keeps that decision runtime — the app
// cannot know the address until someone types it.
api.use({
  async onRequest({ request }) {
    if (!isRemote()) return request;
    const url = new URL(request.url);

    // The body is read out and passed as bytes rather than letting
    // `new Request(url, request)` carry it over. That form gives the copy a
    // ReadableStream body, and WebKit — every webview on macOS, so every Mac
    // desktop install — refuses to upload a stream: "ReadableStream uploading
    // is not supported". Chromium accepts it, so this looked fine everywhere
    // it was tried. The failure was every write from the desktop app going
    // nowhere while reads worked perfectly, which reads as "the button does
    // nothing" rather than as a network bug.
    const hasBody = request.method !== "GET" && request.method !== "HEAD";
    const body = hasBody ? await request.arrayBuffer() : undefined;

    const rewritten = new Request(apiUrl(url.pathname + url.search), {
      method: request.method,
      headers: request.headers,
      body,
      credentials: request.credentials,
      mode: request.mode,
      redirect: request.redirect,
      referrer: request.referrer,
      integrity: request.integrity,
      signal: request.signal,
    });
    for (const [k, v] of Object.entries(authHeaders())) {
      rewritten.headers.set(k, v);
    }
    return rewritten;
  },
});

/** Open a WebSocket against the API origin. Delegates to the single socket
 *  constructor so the auth subprotocol is never omitted. */
export function apiSocket(path: string): WebSocket {
  return openSocket(path);
}

function isWrite(method: string): boolean {
  return method !== "GET" && method !== "HEAD";
}

function pathOf(url: string): string {
  try {
    return new URL(url).pathname;
  } catch {
    return url;
  }
}

api.use({
  async onResponse({ request, response }) {
    if (response.ok || !isWrite(request.method)) return;
    let message = `${response.status} ${response.statusText}`.trim();
    try {
      const text = await response.clone().text();
      if (text) {
        const parsed = JSON.parse(text) as { message?: string; error?: string };
        message = parsed.message ?? parsed.error ?? text.slice(0, 200);
      }
    } catch {
      // A body we cannot read is not worth failing over; the status still says
      // something useful.
    }
    reportWriteFailure({
      method: request.method,
      path: pathOf(request.url),
      status: response.status,
      message,
    });
  },
  onError({ request, error }) {
    // The case that matters most, and the one a status check would miss: the
    // request never left. That is what a WebKit webview does when handed a
    // body it cannot upload, and it is what being offline looks like.
    if (!isWrite(request.method)) return;
    reportWriteFailure({
      method: request.method,
      path: pathOf(request.url),
      message: error instanceof Error ? error.message : String(error),
    });
  },
});
