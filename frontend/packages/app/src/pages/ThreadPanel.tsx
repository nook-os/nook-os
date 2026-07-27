// The thread panel (MAIN-114 AC-5): a right-hand pane hanging off one top-level
// message. It opens the parent's replies with `messageThread` (paginated exactly
// like channel history) and reuses the same reusable ChatView the channel does —
// but WITHOUT `onOpenThread`, so replies are not themselves threadable (NG-1).
//
// It runs NO socket of its own. Chat.tsx keeps a single channel websocket whose
// `live` buffer carries every message for the open channel, replies included; we
// take that buffer as a prop and let `buildThreadMessages` surface just this
// thread's live replies (AC-4) and reconcile our optimistic sends.
import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useInfiniteQuery, useMutation } from "@tanstack/react-query";
import { messageThread, postMessage, type ChatMessage } from "@nookos/api";
import { ChatView } from "@nookos/ui";
import { buildThreadMessages, type PendingMessage } from "./chatMessages";

export interface ThreadPanelProps {
  channelId: string;
  parent: ChatMessage; // the top-level message the thread hangs off
  live: ChatMessage[]; // the channel's live buffer, from Chat.tsx
  meId: string | undefined;
  names: Record<string, string>; // e.g. { [meId]: "You" }
  onClose: () => void;
}

const PAGE_SIZE = 50;

export function ThreadPanel({
  channelId,
  parent,
  live,
  meId,
  names,
  onClose,
}: ThreadPanelProps) {
  const threadQuery = useInfiniteQuery({
    queryKey: ["chat", "thread", parent.id],
    initialPageParam: undefined as string | undefined,
    queryFn: ({ pageParam }) => messageThread(parent.id, pageParam, PAGE_SIZE),
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  });
  const replies: ChatMessage[] = useMemo(
    () => (threadQuery.data?.pages ?? []).flatMap((p) => p.replies),
    [threadQuery.data],
  );

  // Optimistic reply state, mirroring Chat.tsx's channel sends. We hold no `live`
  // of our own — our echo arrives through the shared channel buffer — so the
  // optimistic bubble is reconciled against it inside `buildThreadMessages`.
  const [pending, setPending] = useState<PendingMessage[]>([]);
  const tempCounter = useRef(0);

  // A different parent is a different thread — drop any in-flight optimism.
  useEffect(() => {
    setPending([]);
  }, [parent.id]);

  const sendMutation = useMutation({
    mutationFn: (v: { tempId: string; body: string }) =>
      postMessage(channelId, v.body, parent.id),
    onError: (_err, v) => {
      // `postMessage` already surfaced the failure through the shared path; here
      // we only mark the optimistic bubble for retry. The echo (on success)
      // arrives via the shared channel `live` buffer, so there is nothing to fold.
      setPending((prev) =>
        prev.map((p) => (p.tempId === v.tempId ? { ...p, failed: true } : p)),
      );
    },
  });

  const send = useCallback(
    (body: string, tempId: string) => {
      if (!meId) return;
      sendMutation.mutate({ tempId, body });
    },
    [meId, sendMutation],
  );

  const onSend = useCallback(
    (body: string) => {
      if (!meId) return;
      const tempId = `pending-${tempCounter.current++}`;
      setPending((prev) => [
        ...prev,
        { tempId, authorId: meId, body, createdAt: new Date().toISOString() },
      ]);
      send(body, tempId);
    },
    [meId, send],
  );

  const onRetry = useCallback(
    (message: { id: string; body: string }) => {
      setPending((prev) =>
        prev.map((p) => (p.tempId === message.id ? { ...p, failed: false } : p)),
      );
      send(message.body, message.id);
    },
    [send],
  );

  const messages = useMemo(
    () => buildThreadMessages(replies, live, pending, parent.id, meId, names),
    [replies, live, pending, parent.id, meId, names],
  );

  const parentAuthor =
    names[parent.author_id] ??
    parent.author_name ??
    `${parent.author_id.slice(0, 8)}…`;

  return (
    <aside className="chat-thread-panel" aria-label="Thread">
      <div className="chat-thread-head">
        <span>Thread</span>
        <button
          type="button"
          className="chat-thread-close"
          aria-label="Close thread"
          onClick={onClose}
        >
          ×
        </button>
      </div>
      <div className="chat-thread-parent">
        <span className="chat-author">{parentAuthor}</span>
        <div className="chat-thread-body">{parent.body}</div>
      </div>
      <ChatView
        messages={messages}
        onSend={onSend}
        onLoadOlder={() => void threadQuery.fetchNextPage()}
        hasMore={threadQuery.hasNextPage}
        loadingOlder={threadQuery.isFetchingNextPage}
        currentUserId={meId}
        onRetry={onRetry}
        emptyLabel="No replies yet."
        placeholder="Reply…"
      />
    </aside>
  );
}
