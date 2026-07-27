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
  listDms,
  me as chatMe,
  openDm,
  postMessage,
  type ChatChannel,
  type ChatMessage,
  type DmSummary,
} from "@nookos/api";
import { Plus } from "lucide-react";
import { ChatView } from "@nookos/ui";
import { buildChatMessages, type PendingMessage } from "./chatMessages";
import { ChannelManager } from "./ChannelManager";
import { DmPicker } from "./DmPicker";

/** A DM has no name of its own — label it by its OTHER participants (MAIN-113
 *  AC-5). Falls back to "Direct message" if names haven't resolved yet. */
function dmName(dm: DmSummary, myPersonId: string | null | undefined): string {
  const others = dm.participants
    .filter((p) => p.person_id !== myPersonId)
    .map((p) => p.display_name)
    .filter(Boolean);
  return others.length ? others.join(", ") : "Direct message";
}

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

  // The caller's direct messages (MAIN-113), listed beside the channels.
  const dmsQuery = useQuery({ queryKey: ["chat", "dms"], queryFn: listDms });
  const dms = dmsQuery.data ?? [];

  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [managing, setManaging] = useState(false);
  const [pickingDm, setPickingDm] = useState(false);

  // Auto-select the first channel once the list loads, but never fight a user's
  // choice or point at a conversation (channel OR dm) that has since vanished.
  useEffect(() => {
    if (
      selectedId &&
      (channels.some((c) => c.id === selectedId) || dms.some((d) => d.id === selectedId))
    )
      return;
    if (channels.length > 0) setSelectedId(channels[0].id);
    else if (dms.length > 0) setSelectedId(dms[0].id);
  }, [channels, dms, selectedId]);

  // When the picker opens a DM, jump straight into it.
  const onDmOpened = useCallback(
    (dm: DmSummary) => {
      void qc.invalidateQueries({ queryKey: ["chat", "dms"] });
      setSelectedId(dm.id);
      setPickingDm(false);
    },
    [qc],
  );

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
  const activeDm = dms.find((d) => d.id === selectedId);
  const activeTitle = activeChannel
    ? activeChannel.name
    : activeDm
      ? dmName(activeDm, chatIdentity?.person_id)
      : null;

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

        <div className="chat-channels-head">
          <span>Direct Messages</span>
          <button
            type="button"
            className="chat-channels-manage"
            onClick={() => setPickingDm(true)}
            title="new direct message"
            aria-label="new direct message"
          >
            <Plus size={13} />
          </button>
        </div>
        {dms.length === 0 ? (
          <div className="chat-channels-empty">No direct messages yet.</div>
        ) : (
          dms.map((d) => (
            <button
              key={d.id}
              type="button"
              className={`chat-channel${d.id === selectedId ? " active" : ""}`}
              onClick={() => setSelectedId(d.id)}
            >
              <span className="chat-channel-hash">@</span>
              {dmName(d, chatIdentity?.person_id)}
            </button>
          ))
        )}
      </aside>
      <section className="chat-main">
        <header className="chat-main-head">
          {activeTitle ? (
            <>
              <span className="chat-channel-hash">{activeDm ? "@" : "#"}</span>
              {activeTitle}
              {activeChannel?.owner_type === "org" && (
                <span className="chat-channel-org" title="Shared across your org">
                  org
                </span>
              )}
            </>
          ) : (
            "Select a conversation"
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
          placeholder={activeTitle ? `Message ${activeTitle}` : "Select a conversation"}
          onRetry={onRetry}
        />
      </section>
      {managing && canManage && <ChannelManager onClose={() => setManaging(false)} />}
      {pickingDm && <DmPicker onClose={() => setPickingDm(false)} onOpened={onDmOpened} />}
    </div>
  );
}
