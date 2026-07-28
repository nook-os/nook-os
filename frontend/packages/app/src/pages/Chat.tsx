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
  connectChatStream,
  deleteMessage,
  editMessage,
  listCategories,
  listChannels,
  listDms,
  markRead,
  me as chatMe,
  openDm,
  postMessage,
  toggleReaction,
  updateChannel,
  type ChatChannel,
  type ChatMessage,
  type DmSummary,
} from "@nookos/api";
import { Plus } from "lucide-react";
import { ChatView } from "@nookos/ui";
import {
  applyMessageUpdate,
  buildChatMessages,
  type PendingMessage,
} from "./chatMessages";
import { askConfirm, askText, notify } from "../dialogs";
import { type ContextMenuItem } from "../contextMenu";
import { ChannelManager } from "./ChannelManager";
import { ChannelSidebar } from "./ChannelSidebar";
import { DmPicker } from "./DmPicker";
import { ThreadPanel } from "./ThreadPanel";

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

/** The compact unread label: the real count, capped for display (MAIN-117 AC-5). */
function unreadLabel(n: number): string {
  return n > 99 ? "99+" : String(n);
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

  // Channel management from the sidebar right-click menu (MAIN-177) — the SAME
  // rename dialog + `updateChannel` PATCH the admin modal uses (NG-1), and the
  // same `["chat","channels"]` invalidation, so the sidebar (and the modal)
  // refresh without a reload.
  const renameChannel = useCallback(
    async (c: ChatChannel) => {
      const next = await askText({ title: "Rename channel", label: "Name", value: c.name });
      if (!next || next === c.name) return;
      try {
        await updateChannel(c.id, { name: next });
        qc.invalidateQueries({ queryKey: ["chat", "channels"] });
      } catch (e) {
        await notify("Could not rename", e instanceof Error ? e.message : String(e));
      }
    },
    [qc],
  );
  const setChannelArchived = useCallback(
    async (c: ChatChannel, value: boolean) => {
      try {
        await updateChannel(c.id, { archived: value });
        qc.invalidateQueries({ queryKey: ["chat", "channels"] });
      } catch (e) {
        await notify("Could not update", e instanceof Error ? e.message : String(e));
      }
    },
    [qc],
  );
  // The admin-only right-click items for a channel (MAIN-177 AC-1): Rename +
  // Archive/Unarchive. No delete anywhere (NG-2/AC-4).
  const channelMenuItems = useCallback(
    (c: ChatChannel): ContextMenuItem[] => [
      { label: "Rename…", onSelect: () => void renameChannel(c) },
      {
        label: c.archived ? "Unarchive" : "Archive",
        onSelect: () => void setChannelArchived(c, !c.archived),
      },
    ],
    [renameChannel, setChannelArchived],
  );

  // Each conversation's unread_count rides the channel/DM list (MAIN-117); the
  // live app-wide chat stream bumps badges instantly, with refetch-on-focus and
  // resync-on-reconnect as the safety nets (no polling).
  const channelsQuery = useQuery({
    queryKey: ["chat", "channels"],
    queryFn: () => listChannels(),
    refetchOnWindowFocus: true,
  });
  const channels = channelsQuery.data ?? [];

  // The channel categories (MAIN-178/179): the sidebar groups channels under
  // these, admin-reorderable. Empty for a tenant that hasn't made any.
  const categoriesQuery = useQuery({
    queryKey: ["chat", "categories"],
    queryFn: listCategories,
    refetchOnWindowFocus: true,
  });
  const categories = categoriesQuery.data ?? [];

  // The caller's direct messages (MAIN-113), listed beside the channels.
  const dmsQuery = useQuery({
    queryKey: ["chat", "dms"],
    queryFn: listDms,
    refetchOnWindowFocus: true,
  });
  const dms = dmsQuery.data ?? [];

  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [managing, setManaging] = useState(false);
  const [pickingDm, setPickingDm] = useState(false);
  // The open thread's parent id, or null when no thread panel is showing.
  const [threadParentId, setThreadParentId] = useState<string | null>(null);

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
  // Ids of replies that arrived as NEW posts this session (never updates), so a
  // parent's "N replies" bumps only for genuine new arrivals and an edit/delete/
  // reaction on a pre-existing reply can't inflate it (MAIN-116 review fix).
  const [newReplyIds, setNewReplyIds] = useState<ReadonlySet<string>>(new Set());
  const tempCounter = useRef(0);

  // The current history, reachable from stable callbacks (the socket handler and
  // the fold helpers below capture it) without re-subscribing on every page.
  const historyRef = useRef<ChatMessage[]>(history);
  useEffect(() => {
    historyRef.current = history;
  }, [history]);

  // Upsert a message into `live` by id — the single place an update lands. Used
  // for the REST responses to our OWN reaction/edit/delete, whose `reacted` is
  // viewer-accurate and taken verbatim (MAIN-116).
  const upsertLive = useCallback((msg: ChatMessage) => {
    setLive((prev) => {
      const i = prev.findIndex((m) => m.id === msg.id);
      if (i < 0) return [...prev, msg];
      const copy = [...prev];
      copy[i] = msg;
      return copy;
    });
  }, []);

  // Apply a `message_updated` broadcast (AC-5): merge onto our current copy —
  // from `live` if we already updated it, else the history page — preserving our
  // own `reacted` while taking the broadcast's neutral counts/body/flags.
  const applyBroadcast = useCallback((incoming: ChatMessage) => {
    setLive((prev) => {
      const existing =
        prev.find((m) => m.id === incoming.id) ??
        historyRef.current.find((m) => m.id === incoming.id);
      const merged = applyMessageUpdate(existing, incoming);
      const i = prev.findIndex((m) => m.id === incoming.id);
      if (i < 0) return [...prev, merged];
      const copy = [...prev];
      copy[i] = merged;
      return copy;
    });
  }, []);

  // Reaction/edit/delete actions (AC-2/3/4). Each REST call returns the updated
  // message; we fold it straight into `live` so the actor sees the change at
  // once, while the `message_updated` broadcast carries it to other viewers.
  // Failures already surface through the shared write-failure path.
  const onToggleReaction = useCallback(
    async (messageId: string, emoji: string, on: boolean) => {
      try {
        upsertLive(await toggleReaction(messageId, emoji, on));
      } catch {
        // reported by the shared write-failure path
      }
    },
    [upsertLive],
  );

  const onEditMessage = useCallback(
    async (messageId: string, newBody: string) => {
      try {
        upsertLive(await editMessage(messageId, newBody));
      } catch {
        // reported by the shared write-failure path
      }
    },
    [upsertLive],
  );

  const onDeleteMessage = useCallback(
    async (messageId: string) => {
      const ok = await askConfirm({
        title: "Delete message?",
        description:
          "It will be replaced with a “message deleted” placeholder for everyone. This cannot be undone.",
        confirmLabel: "delete",
        danger: true,
      });
      if (!ok) return;
      try {
        upsertLive(await deleteMessage(messageId));
      } catch {
        // reported by the shared write-failure path
      }
    },
    [upsertLive],
  );

  useEffect(() => {
    setLive([]);
    setPending([]);
    setNewReplyIds(new Set());
    setThreadParentId(null);
  }, [selectedId]);

  // The active conversation, reachable from the always-on stream handler without
  // re-subscribing on every switch.
  const selectedIdRef = useRef<string | null>(selectedId);
  useEffect(() => {
    selectedIdRef.current = selectedId;
  }, [selectedId]);

  // ONE app-wide chat stream for the whole session (MAIN-117 AC-4): every
  // channel/DM the caller belongs to arrives here, tagged by `channel_id`. A
  // message for the OPEN conversation folds into `live`; a message for any OTHER
  // conversation re-syncs the lists so its unread badge bumps at once — instant,
  // no polling. Mounted once (stable deps) and torn down on unmount.
  useEffect(() => {
    const bumpBadges = () => {
      void qc.invalidateQueries({ queryKey: ["chat", "channels"] });
      void qc.invalidateQueries({ queryKey: ["chat", "dms"] });
    };
    const dispose = connectChatStream(
      (msg) => {
        if (msg.channel_id === selectedIdRef.current) {
          setLive((prev) => (prev.some((m) => m.id === msg.id) ? prev : [...prev, msg]));
          // A NEW reply bumps its parent's count; record its id so a later update
          // to it is not re-counted (MAIN-116 review fix).
          if (msg.parent_message_id) {
            setNewReplyIds((prev) => (prev.has(msg.id) ? prev : new Set(prev).add(msg.id)));
          }
        } else {
          bumpBadges();
        }
      },
      {
        onReconnect: () => {
          // Resync everything a gap could have staled: badge counts and the open
          // conversation's history (which carries authoritative reply counts, so
          // drop the optimistic new-reply bumps too).
          setNewReplyIds(new Set());
          bumpBadges();
          if (selectedIdRef.current) {
            void qc.invalidateQueries({ queryKey: ["chat", "messages", selectedIdRef.current] });
          }
        },
        // An edit/soft-delete/reaction (AC-5): fold into the open conversation
        // (replies included, so an open thread updates too); for a background one,
        // re-sync badges since a soft-delete can lower a count.
        onUpdate: (msg) => {
          if (msg.channel_id === selectedIdRef.current) applyBroadcast(msg);
          else bumpBadges();
        },
      },
    );
    return dispose;
  }, [qc, applyBroadcast]);

  // Advance the caller's read cursor for a conversation, then refresh the lists
  // so its badge clears at once (MAIN-117 AC-3). Failures are swallowed — a
  // missed mark-read self-heals on the next focus refetch.
  const markConversationRead = useCallback(
    (id: string) => {
      void markRead(id)
        .then(() => {
          void qc.invalidateQueries({ queryKey: ["chat", "channels"] });
          void qc.invalidateQueries({ queryKey: ["chat", "dms"] });
        })
        .catch(() => {});
    },
    [qc],
  );

  // Mark the open conversation read on open/focus and whenever a new message
  // lands while it is on-screen and the tab is focused — scroll-to-bottom
  // semantics (AC-3). `live.length` resets to 0 on switch, so the first pass is
  // the open trigger; a backgrounded tab is left unread on purpose, and
  // switching away simply stops advancing the cursor.
  useEffect(() => {
    if (!selectedId) return;
    if (typeof document !== "undefined" && !document.hasFocus()) return;
    markConversationRead(selectedId);
  }, [selectedId, live.length, markConversationRead]);

  // Regaining focus with a conversation open re-marks it read (MAIN-117 review
  // should-fix): messages that arrived while the tab was backgrounded left the
  // badge set, and the effect above doesn't re-fire on focus alone.
  useEffect(() => {
    const onFocus = () => {
      if (selectedIdRef.current) markConversationRead(selectedIdRef.current);
    };
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [markConversationRead]);

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
    () => buildChatMessages(history, live, pending, meId, names, newReplyIds),
    [history, live, pending, meId, names, newReplyIds],
  );

  // Resolve the open thread's parent message from what we already hold; a reply
  // is never a valid parent, so it can only match a top-level channel message.
  const threadParent = useMemo(
    () =>
      threadParentId
        ? ([...history, ...live].find((x) => x.id === threadParentId) ?? null)
        : null,
    [threadParentId, history, live],
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
        <ChannelSidebar
          channels={channels}
          categories={categories}
          selectedId={selectedId}
          onSelect={setSelectedId}
          canManage={canManage}
          menuItems={channelMenuItems}
          loading={channelsQuery.isLoading}
        />

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
              {(d.unread_count ?? 0) > 0 && (
                <span className="chat-unread" aria-label={`${d.unread_count} unread`}>
                  {unreadLabel(d.unread_count ?? 0)}
                </span>
              )}
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
        <div className="chat-main-body" style={{ display: "flex", flex: 1, minHeight: 0 }}>
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
            onOpenThread={(m) => setThreadParentId(m.id)}
            onToggleReaction={onToggleReaction}
            onEditMessage={onEditMessage}
            onDeleteMessage={onDeleteMessage}
            canDeleteAny={canManage}
          />
          {threadParent && selectedId && (
            <ThreadPanel
              channelId={selectedId}
              parent={threadParent}
              live={live}
              meId={meId}
              names={names}
              onClose={() => setThreadParentId(null)}
              onToggleReaction={onToggleReaction}
              onEditMessage={onEditMessage}
              onDeleteMessage={onDeleteMessage}
              canDeleteAny={canManage}
            />
          )}
        </div>
      </section>
      {managing && canManage && <ChannelManager onClose={() => setManaging(false)} />}
      {pickingDm && <DmPicker onClose={() => setPickingDm(false)} onOpened={onDmOpened} />}
    </div>
  );
}
