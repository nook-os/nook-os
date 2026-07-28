// Pure message-merge logic for the Chat surface, factored out so it can be
// tested without a DOM or a websocket. The page holds three sources:
//
//   - history: confirmed messages from the paginated REST fetch (each page is
//     newest-first; across pages, still newest-first).
//   - live:    confirmed messages that arrived after load — over the websocket
//     or as the echo of our own POST. Chronological arrival order.
//   - pending: optimistic messages the user just sent, not yet confirmed.
//
// The output is one chronological list (oldest → newest) for `ChatView`, with
// each optimistic message dropped as soon as its server echo appears — so a
// post never double-renders when its own broadcast comes back.
import type { ChatMessage } from "@nookos/api";
import type { ChatViewMessage } from "@nookos/ui";

export interface PendingMessage {
  tempId: string;
  authorId: string;
  body: string;
  createdAt: string;
  failed?: boolean;
}

function toView(
  m: ChatMessage,
  names: Record<string, string>,
  extraReplies = 0,
): ChatViewMessage {
  // Server-side count (from history, AC-3) plus replies that arrived live this
  // session (AC-4). `undefined` when zero so the view shows no affordance.
  const replyCount = (m.reply_count ?? 0) + extraReplies;
  return {
    id: m.id,
    authorId: m.author_id,
    // Prefer the server-resolved display name (works cross-tenant in org
    // channels — MAIN-112 AC-4); fall back to the local map ("You" for me).
    authorName: names[m.author_id] ?? m.author_name ?? undefined,
    body: m.body,
    createdAt: m.created_at,
    replyCount: replyCount > 0 ? replyCount : undefined,
  };
}

/** Sort confirmed messages oldest → newest. UUID v7 ids are time-ordered, so
 *  they break ties deterministically when two share a timestamp. */
function chrono(a: ChatMessage, b: ChatMessage): number {
  if (a.created_at !== b.created_at) return a.created_at < b.created_at ? -1 : 1;
  return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
}

/**
 * Drop each optimistic message whose echo has arrived. An echo is a *live*
 * message authored by me with the same body — one echo cancels one pending, so
 * sending the same text twice still shows two until both echoes land. Failed
 * sends are never cancelled (they produced no echo) and stay for retry. History
 * is deliberately excluded: our new sends echo through `live`, never through the
 * older page we just fetched, so an old identical message can't falsely cancel a
 * fresh optimistic one.
 */
export function reconcilePending(
  pending: PendingMessage[],
  liveFromMe: ChatMessage[],
): PendingMessage[] {
  const counts: Record<string, number> = {};
  for (const m of liveFromMe) counts[m.body] = (counts[m.body] ?? 0) + 1;
  const remaining: PendingMessage[] = [];
  for (const p of pending) {
    if (!p.failed && (counts[p.body] ?? 0) > 0) {
      counts[p.body] -= 1;
      continue;
    }
    remaining.push(p);
  }
  return remaining;
}

/** Merge the three sources into the chronological list `ChatView` renders.
 *
 *  Threaded replies (a message with `parent_message_id`) never appear in this
 *  channel stream — they live only under their parent's thread (MAIN-114 AC-2).
 *  They still arrive over the channel socket, so each live reply instead bumps
 *  its parent's reply count here (AC-4); the thread panel renders the reply
 *  itself. */
export function buildChatMessages(
  history: ChatMessage[],
  live: ChatMessage[],
  pending: PendingMessage[],
  meId: string | undefined,
  names: Record<string, string> = {},
): ChatViewMessage[] {
  // Dedupe confirmed messages by id: a message can appear in both a refetched
  // history page and the live buffer.
  const byId = new Map<string, ChatMessage>();
  for (const m of history) byId.set(m.id, m);
  for (const m of live) byId.set(m.id, m);

  // Count replies we know about live, per parent, to bump the parent's count.
  // History carries no replies (the server excludes them), so this counts only
  // this session's live arrivals — added on top of the server `reply_count`.
  const liveReplies: Record<string, number> = {};
  for (const m of byId.values()) {
    if (m.parent_message_id) {
      liveReplies[m.parent_message_id] = (liveReplies[m.parent_message_id] ?? 0) + 1;
    }
  }

  // Only top-level messages render in the channel stream (AC-2).
  const confirmed = [...byId.values()]
    .filter((m) => !m.parent_message_id)
    .sort(chrono);

  // Reconcile only against my own TOP-LEVEL live echoes — a reply I posted must
  // never cancel an optimistic channel message that happens to share its body.
  const liveFromMe = meId
    ? live.filter((m) => m.author_id === meId && !m.parent_message_id)
    : [];
  const remaining = reconcilePending(pending, liveFromMe);

  return [
    ...confirmed.map((m) => toView(m, names, liveReplies[m.id] ?? 0)),
    ...pendingViews(remaining, names),
  ];
}

/** Optimistic bubbles → view messages, marked pending (or failed for retry). */
function pendingViews(
  remaining: PendingMessage[],
  names: Record<string, string>,
): ChatViewMessage[] {
  return remaining.map((p) => ({
    id: p.tempId,
    authorId: p.authorId,
    authorName: names[p.authorId],
    body: p.body,
    createdAt: p.createdAt,
    pending: !p.failed,
    failed: p.failed,
  }));
}

/** Merge a thread's replies for the ChatView inside the thread panel (MAIN-114
 *  AC-5). The INVERSE of `buildChatMessages`: it keeps ONLY replies to
 *  `parentId` — never the parent, never other threads' replies, never the
 *  top-level channel chatter that shares the same live buffer. Replies are not
 *  themselves threadable (NG-1), so the caller omits `onOpenThread` on this
 *  ChatView; no reply affordance is emitted here. */
export function buildThreadMessages(
  replies: ChatMessage[],
  live: ChatMessage[],
  pending: PendingMessage[],
  parentId: string,
  meId: string | undefined,
  names: Record<string, string> = {},
): ChatViewMessage[] {
  const byId = new Map<string, ChatMessage>();
  for (const m of replies) byId.set(m.id, m);
  // The channel socket carries every message; take only this thread's replies.
  for (const m of live) {
    if (m.parent_message_id === parentId) byId.set(m.id, m);
  }
  const confirmed = [...byId.values()]
    .filter((m) => m.parent_message_id === parentId)
    .sort(chrono);

  const liveFromMe = meId
    ? live.filter((m) => m.author_id === meId && m.parent_message_id === parentId)
    : [];
  const remaining = reconcilePending(pending, liveFromMe);

  return [...confirmed.map((m) => toView(m, names)), ...pendingViews(remaining, names)];
}
