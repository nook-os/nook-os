// Client for the team-chat service. Chat is a separate origin fronted by the
// `/chat` proxy, so it is NOT part of the control-plane OpenAPI document and
// cannot ride the typed `api` client. This is the one hand-rolled client — kept
// thin, typed against the Rust-owned chat DTOs, and routed through the same
// auth (`authHeaders` / the `openSocket` subprotocol) and the same
// write-failure path as everything else.
import type { components } from "./generated/schema";
import { apiUrl, authHeaders, openSocket } from "./endpoint";
import { reportWriteFailure } from "./write-failure";

type Schemas = components["schemas"];
type ChatChannel = Schemas["ChatChannel"];
type ChatMessage = Schemas["ChatMessage"];
type ChatMessagePage = Schemas["ChatMessagePage"];
export type ChatThread = Schemas["ChatThread"];
type CreateChatChannel = Schemas["CreateChatChannel"];
type UpdateChatChannel = Schemas["UpdateChatChannel"];
export type DmSummary = Schemas["DmSummary"];
export type PersonRef = Schemas["PersonRef"];

/** `GET /api/me` — the caller as chat resolves them, including their tenant
 *  `role` so the UI can gate channel management to admins (MAIN-94 AC-5). */
export interface ChatMe {
  user_id: string;
  tenant_id: string;
  /** The caller's cross-tenant person id — used to name a DM by its other
   *  participants (MAIN-113). `null` for a user row with no person. */
  person_id: string | null;
  cookie_session: boolean;
  role: string | null;
}

// Everything hangs off the `/chat` proxy path — the same rewrite `apiUrl`
// applies to `/api` in the desktop build carries chat along with it.
const CHAT_PREFIX = "/chat/api";

async function chatGet<T>(path: string): Promise<T> {
  const res = await fetch(apiUrl(`${CHAT_PREFIX}${path}`), {
    method: "GET",
    headers: { ...authHeaders() },
    credentials: "same-origin",
  });
  if (!res.ok) {
    throw new Error(`chat GET ${path} failed: ${res.status} ${res.statusText}`);
  }
  return (await res.json()) as T;
}

/**
 * A write against the chat service (POST/PATCH), routed through the same
 * `authHeaders` and the shared write-failure path as `postMessage`, and
 * rethrowing so a caller can surface an inline error. Mirrors the control-plane
 * client's write handling so a failed channel edit looks like any other.
 */
async function chatWrite<T>(
  method: "POST" | "PATCH",
  path: string,
  body: unknown,
): Promise<T> {
  const full = `${CHAT_PREFIX}${path}`;
  let res: Response;
  try {
    res = await fetch(apiUrl(full), {
      method,
      headers: { ...authHeaders(), "Content-Type": "application/json" },
      credentials: "same-origin",
      body: JSON.stringify(body),
    });
  } catch (err) {
    reportWriteFailure({
      method,
      path: full,
      message: err instanceof Error ? err.message : String(err),
    });
    throw err;
  }
  if (!res.ok) {
    let message = `${res.status} ${res.statusText}`.trim();
    try {
      const text = await res.clone().text();
      if (text) {
        const parsed = JSON.parse(text) as { message?: string; error?: string };
        message = parsed.message ?? parsed.error ?? text.slice(0, 200);
      }
    } catch {
      // A body we cannot read is not worth failing over.
    }
    reportWriteFailure({ method, path: full, status: res.status, message });
    throw new Error(message);
  }
  return (await res.json()) as T;
}

/**
 * List the tenant's channels. Archived channels are excluded by default (what
 * the sidebar wants); pass `includeArchived` for the management modal's
 * archived view (MAIN-94 AC-6).
 */
export function listChannels(includeArchived = false): Promise<ChatChannel[]> {
  const q = includeArchived ? "?include_archived=true" : "";
  return chatGet<ChatChannel[]>(`/channels${q}`);
}

/** The caller, including their tenant role (MAIN-94 AC-5). */
export function me(): Promise<ChatMe> {
  return chatGet<ChatMe>("/me");
}

/**
 * Create a channel (admin only server-side; the UI hides it for non-admins).
 * `owner` is "tenant" (default — shared by your team) or "org" (shared across
 * every tenant under your org — MAIN-112).
 */
export function createChannel(
  name: string,
  owner: "tenant" | "org" = "tenant",
): Promise<ChatChannel> {
  const body: CreateChatChannel = { name, owner };
  return chatWrite<ChatChannel>("POST", "/channels", body);
}

/** The caller's direct-message conversations, newest first (MAIN-113). */
export function listDms(): Promise<DmSummary[]> {
  return chatGet<DmSummary[]>("/dms");
}

/**
 * Open — or reuse — a DM with `personIds` (the caller is added automatically).
 * Opening the same set twice returns the same conversation, never a duplicate.
 */
export function openDm(personIds: string[]): Promise<DmSummary> {
  return chatWrite<DmSummary>("POST", "/dms", { person_ids: personIds });
}

/** People the caller may start a DM with — everyone in their org(s) (MAIN-113). */
export function listPeople(): Promise<PersonRef[]> {
  return chatGet<PersonRef[]>("/people");
}

/** Rename or (un)archive a channel (admin only server-side). */
export function updateChannel(
  id: string,
  patch: UpdateChatChannel,
): Promise<ChatChannel> {
  return chatWrite<ChatChannel>("PATCH", `/channels/${id}`, patch);
}

/**
 * A page of a channel's history, newest first. Pass the previous page's
 * `next_cursor` as `before` to walk backwards into older messages; the walk is
 * over when `next_cursor` comes back null.
 */
export function channelHistory(
  channelId: string,
  before?: string | null,
  limit = 50,
): Promise<ChatMessagePage> {
  const params = new URLSearchParams({ limit: String(limit) });
  if (before) params.set("before", before);
  return chatGet<ChatMessagePage>(`/channels/${channelId}/messages?${params}`);
}

/**
 * A page of a message's thread (MAIN-114): the parent plus its replies,
 * newest-first, keyset-paginated exactly like channel history — pass the
 * previous page's `next_cursor` as `before` to walk into older replies.
 */
export function messageThread(
  messageId: string,
  before?: string | null,
  limit = 50,
): Promise<ChatThread> {
  const params = new URLSearchParams({ limit: String(limit) });
  if (before) params.set("before", before);
  return chatGet<ChatThread>(`/messages/${messageId}/thread?${params}`);
}

/**
 * Post a message. A failure is reported through the shared write-failure path
 * (so it surfaces like any other failed write) AND rethrown, so an optimistic
 * caller can roll back and offer a retry.
 *
 * Pass `parentMessageId` to post a threaded reply (MAIN-114) — the server
 * requires the parent to be in this same channel and to be top-level itself.
 */
export async function postMessage(
  channelId: string,
  body: string,
  parentMessageId?: string,
): Promise<ChatMessage> {
  const path = `${CHAT_PREFIX}/channels/${channelId}/messages`;
  let res: Response;
  try {
    res = await fetch(apiUrl(path), {
      method: "POST",
      headers: { ...authHeaders(), "Content-Type": "application/json" },
      credentials: "same-origin",
      body: JSON.stringify(
        parentMessageId ? { body, parent_message_id: parentMessageId } : { body },
      ),
    });
  } catch (err) {
    // The request never left — offline, or a webview refusing the body.
    reportWriteFailure({
      method: "POST",
      path,
      message: err instanceof Error ? err.message : String(err),
    });
    throw err;
  }
  if (!res.ok) {
    let message = `${res.status} ${res.statusText}`.trim();
    try {
      const text = await res.clone().text();
      if (text) {
        const parsed = JSON.parse(text) as { message?: string; error?: string };
        message = parsed.message ?? parsed.error ?? text.slice(0, 200);
      }
    } catch {
      // A body we cannot read is not worth failing over.
    }
    reportWriteFailure({ method: "POST", path, status: res.status, message });
    throw new Error(message);
  }
  return (await res.json()) as ChatMessage;
}

/**
 * Open the channel's live socket. Mirrors `openSocket` so the auth subprotocol
 * is reused — the desktop app authenticates its sockets by bearer token, not a
 * cookie, and this is the only way that token reaches the chat WS.
 *
 * Prefer `connectChatSocket` in the app; this raw opener exists for tests and
 * for callers that manage their own lifecycle.
 */
export function openChatSocket(channelId: string): WebSocket {
  return openSocket(`${CHAT_PREFIX}/channels/${channelId}/ws`);
}

/**
 * Subscribe to a channel's live messages with automatic reconnect + backoff,
 * mirroring `connectUiSocket`. Returns a disposer that permanently closes the
 * socket — call it when switching channels or unmounting, so connections never
 * leak. The server replays nothing on reconnect; `onReconnect` is the caller's
 * cue to refetch recent history and close any gap opened while the socket was
 * down.
 */
export function connectChatSocket(
  channelId: string,
  onMessage: (message: ChatMessage) => void,
  handlers?: { onOpen?: () => void; onReconnect?: () => void; onClose?: () => void },
): () => void {
  let closed = false;
  let socket: WebSocket | null = null;
  let backoff = 1000;
  let first = true;

  const open = () => {
    if (closed) return;
    socket = openChatSocket(channelId);
    socket.onopen = () => {
      backoff = 1000;
      handlers?.onOpen?.();
      if (!first) handlers?.onReconnect?.();
      first = false;
    };
    socket.onmessage = (e) => {
      try {
        const frame = JSON.parse(e.data) as Schemas["ChatServerMessage"];
        if (frame.type === "message") onMessage(frame.data);
      } catch {
        // ignore malformed frames
      }
    };
    socket.onclose = () => {
      if (closed) return;
      handlers?.onClose?.();
      setTimeout(open, backoff);
      backoff = Math.min(backoff * 2, 15000);
    };
  };
  open();
  return () => {
    closed = true;
    socket?.close();
  };
}
