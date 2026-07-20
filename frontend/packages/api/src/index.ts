// Thin, fully-typed API client. All types are generated from the Rust
// OpenAPI document — regenerate with `./scripts/gen-types.sh`.
import createClient from "openapi-fetch";
import type { paths, components } from "./generated/schema";

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

// Same-origin in dev (Vite proxies /api to the control plane) and in
// production (control plane serves or fronts the app).
export const api = createClient<paths>({
  baseUrl: "/",
  credentials: "include",
});

/** Open a WebSocket against the API origin, ws/wss chosen from the page. */
export function apiSocket(path: string): WebSocket {
  const proto = window.location.protocol === "https:" ? "wss" : "ws";
  return new WebSocket(`${proto}://${window.location.host}${path}`);
}
