// The Chat surface: a channel list beside the reusable ChatView, wired to the
// chat service. REST (TanStack Query) owns durable history; a websocket feeds
// live messages into local state that is merged with history and optimistic
// sends by `buildChatMessages`. The view component knows none of this — see
// `@nookos/ui`'s ChatView for the backend-agnostic contract this page fulfils.
import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  useInfiniteQuery,
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { Settings } from "lucide-react";
import {
  api,
  channelHistory,
  connectChatSocket,
  listChannels,
  me as chatMe,
  postMessage,
  type ChatChannel,
  type ChatMessage,
} from "@nookos/api";
import { ChatView } from "@nookos/ui";
import { buildChatMessages, type PendingMessage } from "./chatMessages";
import { ChannelManager } from "./ChannelManager";

/** Owner and admin manage channels; everyone else reads and posts. Mirrors the
 *  chat service's gate, so the UI never shows a control the server would 403. */
function isAdminRole(role: string | null | undefined): boolean {
  return role === "owner" || role === "admin";
}

const PAGE_SIZE = 50;

export function ChatPage() {
  const qc = useQueryClient();

  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
  const meId = me?.user.id;

  // The caller's tenant role, from chat's own /api/me, gates the management
  // affordances (AC-5). A non-admin simply never sees them.
  const { data: chatIdentity } = useQuery({
    queryKey: ["chat", "me"],
    queryFn: chatMe,
  });
  const canManage = isAdminRole(chatIdentity?.role);

  const channelsQuery = useQuery({
    queryKey: ["chat", "channels"],
    queryFn: () => listChannels(),
  });
  const channels = channelsQuery.data ?? [];

  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [managing, setManaging] = useState(false);

  // Auto-select the first channel once the list loads, but never fight a user's
  // choice or point at a channel that has since vanished.
  useEffect(() => {
    if (channels.length === 0) return;
    if (selectedId && channels.some((c) => c.id === selectedId)) return;
    setSelectedId(channels[0].id);
  }, [channels, selectedId]);

  const historyQuery = useInfiniteQuery({
    queryKey: ["chat", "messages", selectedId],
    enabled: !!selectedId,
    initialPageParam: undefined as string | undefined,
    queryFn: ({ pageParam }) => channelHistory(selectedId!, pageParam, PAGE_SIZE),
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  });
  const history: ChatMessage[] = useMemo(
    () => (historyQuery.data?.pages ?? []).flatMap((p) => p.messages),
    [historyQuery.data],
  );

  // Live + optimistic state is per-open-channel; both reset on a channel switch.
  const [live, setLive] = useState<ChatMessage[]>([]);
  const [pending, setPending] = useState<PendingMessage[]>([]);
  const tempCounter = useRef(0);

  useEffect(() => {
    setLive([]);
    setPending([]);
  }, [selectedId]);

  // One socket per open channel, torn down on switch/unmount so nothing leaks.
  // A reconnect may have missed messages, so refetch history to close the gap.
  useEffect(() => {
    if (!selectedId) return;
    const dispose = connectChatSocket(
      selectedId,
      (msg) => setLive((prev) => (prev.some((m) => m.id === msg.id) ? prev : [...prev, msg])),
      {
        onReconnect: () => {
          void qc.invalidateQueries({ queryKey: ["chat", "messages", selectedId] });
        },
      },
    );
    return dispose;
  }, [selectedId, qc]);

  const sendMutation = useMutation({
    mutationFn: (v: { tempId: string; body: string }) =>
      postMessage(selectedId!, v.body),
    onSuccess: (echo) => {
      // Fold the server's copy into live so the message is confirmed even if the
      // websocket echo is slow or absent; `reconcilePending` then drops the
      // matching optimistic entry.
      setLive((prev) => (prev.some((m) => m.id === echo.id) ? prev : [...prev, echo]));
    },
    onError: (_err, v) => {
      // The failure was already surfaced through the shared write-failure path
      // by `postMessage`; here we only mark the optimistic bubble for retry.
      setPending((prev) =>
        prev.map((p) => (p.tempId === v.tempId ? { ...p, failed: true } : p)),
      );
    },
  });

  const send = useCallback(
    (body: string, tempId: string) => {
      if (!selectedId || !meId) return;
      sendMutation.mutate({ tempId, body });
    },
    [selectedId, meId, sendMutation],
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

  const names = useMemo(() => (meId ? { [meId]: "You" } : {}), [meId]);
  const messages = useMemo(
    () => buildChatMessages(history, live, pending, meId, names),
    [history, live, pending, meId, names],
  );

  const activeChannel = channels.find((c) => c.id === selectedId);

  return (
    <div className="chat-page">
      <aside className="chat-channels" aria-label="Channels">
        <div className="chat-channels-head">
          <span>Channels</span>
          {canManage && (
            <button
              type="button"
              className="chat-channels-manage"
              onClick={() => setManaging(true)}
              title="manage channels"
              aria-label="manage channels"
            >
              <Settings size={13} />
            </button>
          )}
        </div>
        {channelsQuery.isLoading ? (
          <div className="chat-channels-empty">Loading…</div>
        ) : channels.length === 0 ? (
          <div className="chat-channels-empty">No channels yet.</div>
        ) : (
          channels.map((c: ChatChannel) => (
            <button
              key={c.id}
              type="button"
              className={`chat-channel${c.id === selectedId ? " active" : ""}`}
              onClick={() => setSelectedId(c.id)}
            >
              <span className="chat-channel-hash">#</span>
              {c.name}
              {c.owner_type === "org" && (
                <span className="chat-channel-org" title="Shared across your org">
                  org
                </span>
              )}
            </button>
          ))
        )}
      </aside>
      <section className="chat-main">
        <header className="chat-main-head">
          {activeChannel ? (
            <>
              <span className="chat-channel-hash">#</span>
              {activeChannel.name}
              {activeChannel.owner_type === "org" && (
                <span className="chat-channel-org" title="Shared across your org">
                  org
                </span>
              )}
            </>
          ) : (
            "Select a channel"
          )}
        </header>
        <ChatView
          messages={messages}
          onSend={onSend}
          onLoadOlder={() => void historyQuery.fetchNextPage()}
          hasMore={historyQuery.hasNextPage}
          loadingOlder={historyQuery.isFetchingNextPage}
          currentUserId={meId}
          disabled={!selectedId}
          placeholder={activeChannel ? `Message #${activeChannel.name}` : "Select a channel"}
          onRetry={onRetry}
        />
      </section>
      {managing && canManage && <ChannelManager onClose={() => setManaging(false)} />}
    </div>
  );
}
