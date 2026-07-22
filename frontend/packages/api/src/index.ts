// Thin, fully-typed API client. All types are generated from the Rust
// OpenAPI document — regenerate with `./scripts/gen-types.sh`.
import createClient from "openapi-fetch";
import type { paths, components } from "./generated/schema";
import { apiUrl, authHeaders, isRemote, socketProtocols, socketUrl } from "./endpoint";

export type Schemas = components["schemas"];
export type Tenant = Schemas["Tenant"];
export type User = Schemas["User"];
export type MeResponse = Schemas["MeResponse"];
export type Capabilities = Schemas["Capabilities"];
export type NodeInfo = Schemas["Node"];
export type Workspace = Schemas["Workspace"];
export type WorkspaceLocation = Schemas["WorkspaceLocation"];
export type Session = Schemas["Session"];
export type Board = Schemas["Board"];
export type BoardColumn = Schemas["BoardColumn"];
export type TaskItem = Schemas["TaskItem"];
export type EventItem = Schemas["Event"];
export type Note = Schemas["Note"];
export type Theme = Schemas["Theme"];
export type DispatchSuggestion = Schemas["DispatchSuggestion"];

export type { paths };
export * from "./ws";
export * from "./endpoint";

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
  onRequest({ request }) {
    if (!isRemote()) return request;
    const url = new URL(request.url);
    const rewritten = new Request(apiUrl(url.pathname + url.search), request);
    for (const [k, v] of Object.entries(authHeaders())) {
      rewritten.headers.set(k, v);
    }
    return rewritten;
  },
});

/** Open a WebSocket against the API origin. */
export function apiSocket(path: string): WebSocket {
  return new WebSocket(socketUrl(path), socketProtocols());
}
