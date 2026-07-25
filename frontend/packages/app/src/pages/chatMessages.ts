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

function toView(m: ChatMessage, names: Record<string, string>): ChatViewMessage {
  return {
    id: m.id,
    authorId: m.author_id,
    authorName: names[m.author_id],
    body: m.body,
    createdAt: m.created_at,
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

/** Merge the three sources into the chronological list `ChatView` renders. */
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
  const confirmed = [...byId.values()].sort(chrono);

  const liveFromMe = meId ? live.filter((m) => m.author_id === meId) : [];
  const remaining = reconcilePending(pending, liveFromMe);

  const pendingViews: ChatViewMessage[] = remaining.map((p) => ({
    id: p.tempId,
    authorId: p.authorId,
    authorName: names[p.authorId],
    body: p.body,
    createdAt: p.createdAt,
    pending: !p.failed,
    failed: p.failed,
  }));

  return [...confirmed.map((m) => toView(m, names)), ...pendingViews];
}
