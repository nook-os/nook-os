// A reusable, backend-agnostic chat surface: a scroll-back message list plus a
// plain-text composer. It knows nothing about the chat service — it takes
// messages and three callbacks, so a second consumer (the planned tmux "sugar"
// overlay) can drive it from an entirely different data source without touching
// this file.
//
// The contract, in full:
//   - `messages` are in CHRONOLOGICAL order (oldest first, newest last). The
//     component renders newest at the bottom and never reorders them.
//   - `onSend(body)` fires when the user submits non-empty text. The caller
//     owns optimistic insertion; this component only clears the composer.
//   - `onLoadOlder()` fires once when the user scrolls to the top AND `hasMore`
//     is set AND no load is already in flight. Prepend the older page to
//     `messages`; scroll position is preserved across the prepend, so there is
//     no jump.
//   - a "live message" is not a separate prop: appending it to `messages` is
//     how it arrives. If the viewer is near the bottom the view follows it,
//     otherwise their scroll position is left alone.
// Everything else (channel list, websockets, dedupe) belongs to the caller.

import React, { useCallback, useLayoutEffect, useRef, useState } from "react";
import { Copy, MoreHorizontal, Pencil, Reply, SmilePlus, Trash2 } from "lucide-react";
import { ALLOWED_REACTIONS } from "@nookos/api";
import { useAnchoredMenu } from "./useAnchoredMenu";

/** One emoji's tally on a message: how many reacted and whether the viewer is
 *  one of them (so a click toggles it off). Structurally the chat service's
 *  `ChatReactionAggregate`, but named locally to keep the view backend-agnostic. */
export interface ChatViewReaction {
  emoji: string;
  count: number;
  reacted: boolean;
}

/** The minimal message shape the view needs. Deliberately not the chat
 *  service's DTO — any source that can produce these fields can drive it. */
export interface ChatViewMessage {
  id: string;
  authorId: string;
  /** Display name; falls back to a shortened author id when absent. */
  authorName?: string;
  body: string;
  /** ISO-8601 timestamp. */
  createdAt: string;
  /** Optimistically shown, not yet confirmed by the server. */
  pending?: boolean;
  /** The send failed; offer a retry. */
  failed?: boolean;
  /** How many threaded replies hang off this message (MAIN-114). When > 0 the
   *  view shows a "N replies" affordance that opens the thread. */
  replyCount?: number;
  /** Reaction tallies (MAIN-116 AC-2). Absent/empty → no pill row. */
  reactions?: ChatViewReaction[];
  /** The body was edited (MAIN-116 AC-3) → an "(edited)" marker. */
  edited?: boolean;
  /** Soft-deleted (MAIN-116 AC-4): render a placeholder, suppress reactions and
   *  per-message actions. The body already arrives redacted. */
  deleted?: boolean;
}

export interface ChatViewProps {
  messages: ChatViewMessage[];
  onSend: (body: string) => void;
  onLoadOlder?: () => void;
  /** Older pages remain; enables the scroll-to-top load. */
  hasMore?: boolean;
  /** A load is in flight — suppresses re-triggering and shows a hint. */
  loadingOlder?: boolean;
  /** The viewer, so their own messages can be marked/aligned. */
  currentUserId?: string;
  /** Disable the composer (e.g. no channel selected). */
  disabled?: boolean;
  placeholder?: string;
  /** Shown when there are no messages. */
  emptyLabel?: string;
  /** Retry a failed optimistic send. */
  onRetry?: (message: ChatViewMessage) => void;
  /** Open a message's thread (MAIN-114). When set, each confirmed message offers
   *  "Reply in thread" from its hover bar, and a "N replies" affordance under it
   *  when it has any. Omit it — as the thread panel's own reply list does — to
   *  render a plain list with no per-message thread actions (no nesting, NG-1). */
  onOpenThread?: (message: ChatViewMessage) => void;
  /** Toggle a reaction (MAIN-116 AC-2). `on` is the desired next state: clicking
   *  an un-highlighted pill or picking from the "add reaction" menu calls it
   *  with `true`; clicking a highlighted pill calls it with `false`. Available
   *  to everyone; omit to render read-only reactions with no picker. */
  onToggleReaction?: (messageId: string, emoji: string, on: boolean) => void;
  /** Save an inline edit (MAIN-116 AC-3). Only offered on the viewer's own,
   *  non-deleted messages; omit to suppress the Edit action entirely. */
  onEditMessage?: (messageId: string, newBody: string) => void;
  /** Delete a message (MAIN-116 AC-4). Offered on the viewer's own messages, and
   *  on any message when `canDeleteAny` is set (a tenant admin). The caller owns
   *  any confirmation. Omit to suppress. */
  onDeleteMessage?: (messageId: string) => void;
  /** The viewer is a tenant owner/admin, so Delete is offered on ANY message,
   *  not only their own (MAIN-116 AC-4). Edit stays author-only regardless. */
  canDeleteAny?: boolean;
  /** Someone (or something) is composing right now — the label to show under
   *  the log, e.g. "the operator agent is working…" (MAIN-237 AC-1/AC-4).
   *
   *  A plain signal, not a presence model: the loop feeds it from its job state
   *  today, and team-chat presence (MAIN-115) will feed the same prop later, so
   *  the two surfaces show the same affordance without this component learning
   *  what either of them means. Null/absent renders nothing at all.
   *
   *  It sits INSIDE the scroll log so the follow-the-bottom behaviour carries it
   *  into view like a message would; a fixed banner would sit still while the
   *  conversation moved. */
  typing?: string | null;
  /** Rendered between the log and the composer — where a caller puts controls
   *  that belong to the reply rather than to the conversation (the loop's
   *  interaction choice buttons). Team chat passes nothing and is unchanged. */
  beforeComposer?: React.ReactNode;
  /** Hide the composer entirely, for a surface that is read-only right now (a
   *  finished loop job). Distinct from `disabled`, which shows the box greyed:
   *  when there is nothing to say TO, an inert box is just clutter. */
  hideComposer?: boolean;
}

const GROUP_GAP_MS = 5 * 60 * 1000;
const NEAR_BOTTOM_PX = 80;
const TOP_TRIGGER_PX = 40;

function timeLabel(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function authorLabel(m: ChatViewMessage): string {
  return m.authorName ?? `${m.authorId.slice(0, 8)}…`;
}

/** First of a run from one author within the grouping window → show a header. */
function startsGroup(m: ChatViewMessage, prev: ChatViewMessage | undefined): boolean {
  if (!prev) return true;
  if (prev.authorId !== m.authorId) return true;
  return new Date(m.createdAt).getTime() - new Date(prev.createdAt).getTime() > GROUP_GAP_MS;
}

/** Roughly how big each popup is, so it flips upward near the bottom of the log
 *  and stays clear of the right edge instead of opening off-screen. Both are
 *  fixed sizes in the stylesheet — keep these in step with `.chat-react-picker`
 *  and `.chat-more-menu`. The bar's buttons are far narrower than either menu,
 *  so without the width the clamp has nothing useful to work from. */
const PICKER_HEIGHT = 76;
const PICKER_WIDTH = 168;
const MORE_HEIGHT = 84;
const MORE_WIDTH = 132;

/** Put a message's text on the clipboard. Best-effort on purpose: a browser that
 *  refuses (insecure origin, no permission) leaves the body on screen to select
 *  by hand, which is what the user had before this action existed. */
function copyText(body: string): void {
  void navigator.clipboard?.writeText(body)?.catch(() => {});
}

/**
 * One message's actions, floating over its upper-right corner (MAIN-300).
 *
 * They used to sit in a row UNDER the body — always in flow, so every message
 * reserved that height whether or not anyone was looking at it, and an
 * always-present Delete ate row width. The bar is absolutely positioned instead:
 * at rest it costs no height and no width, which is the whole of the density fix.
 *
 * It is its own component because each row owns two popups, and hooks cannot be
 * called from inside the message `map`. Both popups portal out through
 * `useAnchoredMenu` — the bar lives inside `.chat-log`, which scrolls, and an
 * absolutely-positioned menu in there is clipped by its edges.
 */
function MessageActions({
  message,
  canReact,
  canReply,
  canEdit,
  canDelete,
  canCopy,
  onToggleReaction,
  onBeginEdit,
  onDeleteMessage,
  onOpenThread,
}: {
  message: ChatViewMessage;
  canReact: boolean;
  canReply: boolean;
  canEdit: boolean;
  canDelete: boolean;
  canCopy: boolean;
  onToggleReaction?: (messageId: string, emoji: string, on: boolean) => void;
  onBeginEdit: (m: ChatViewMessage) => void;
  onDeleteMessage?: (messageId: string) => void;
  onOpenThread?: (m: ChatViewMessage) => void;
}) {
  const [picker, setPicker] = useState(false);
  const [more, setMore] = useState(false);
  const reactBtn = useRef<HTMLButtonElement>(null);
  const moreBtn = useRef<HTMLButtonElement>(null);

  const closePicker = useCallback(() => setPicker(false), []);
  const closeMore = useCallback(() => setMore(false), []);
  const pick = useAnchoredMenu(picker, closePicker, {
    height: PICKER_HEIGHT,
    width: PICKER_WIDTH,
  });
  const menu = useAnchoredMenu(more, closeMore, {
    height: MORE_HEIGHT,
    width: MORE_WIDTH,
  });

  // Esc closes and hands focus back to the button that opened it — the menu is
  // portalled into `document.body`, so without this focus would be stranded at
  // the end of the document.
  const dismissOn = (
    close: () => void,
    back: React.RefObject<HTMLButtonElement>,
  ) => (e: React.KeyboardEvent) => {
    if (e.key !== "Escape") return;
    e.preventDefault();
    close();
    back.current?.focus();
  };

  // Built as data so the FIRST item — whichever survived the permission checks —
  // can carry `autoFocus`. Opening has to land focus inside the menu or a
  // keyboard user reaches the trigger and then has nowhere to go, and Esc has no
  // focus to hand back (AC-3). It cannot be done from an effect: the portal
  // renders `null` on the pass that opens it, while it measures where to sit.
  const moreItems: {
    key: string;
    label: string;
    aria: string;
    icon: React.ReactNode;
    danger?: boolean;
    run: () => void;
  }[] = [];
  if (canEdit) {
    moreItems.push({
      key: "edit",
      label: "Edit",
      aria: "Edit message",
      icon: <Pencil size={12} />,
      run: () => onBeginEdit(message),
    });
  }
  if (canCopy) {
    moreItems.push({
      key: "copy",
      label: "Copy text",
      aria: "Copy text",
      icon: <Copy size={12} />,
      run: () => copyText(message.body),
    });
  }
  if (canDelete) {
    moreItems.push({
      key: "delete",
      label: "Delete",
      aria: "Delete message",
      icon: <Trash2 size={12} />,
      danger: true,
      run: () => onDeleteMessage?.(message.id),
    });
  }

  return (
    // `.open` keeps the bar visible while one of its menus is torn off into the
    // body portal, where neither `:hover` on the row nor `:focus-within` on the
    // bar can still see it.
    <div className={`chat-msg-bar${picker || more ? " open" : ""}`}>
      {canReact && (
        <div ref={pick.hostRef} className="chat-bar-wrap">
          <button
            ref={reactBtn}
            type="button"
            className={`chat-bar-btn${picker ? " on" : ""}`}
            aria-label="Add reaction"
            aria-haspopup="menu"
            aria-expanded={picker}
            onClick={() => setPicker((v) => !v)}
          >
            <SmilePlus size={13} />
          </button>
          {pick.portal(
            ALLOWED_REACTIONS.map((emoji, i) => (
              <button
                key={emoji}
                type="button"
                className="chat-react-opt"
                role="menuitem"
                autoFocus={i === 0}
                aria-label={`React with ${emoji}`}
                onClick={() => {
                  setPicker(false);
                  onToggleReaction?.(message.id, emoji, true);
                }}
              >
                {emoji}
              </button>
            )),
            "chat-react-picker",
            { role: "menu", onKeyDown: dismissOn(closePicker, reactBtn) },
          )}
        </div>
      )}
      {canReply && (
        <button
          type="button"
          className="chat-bar-btn"
          aria-label="Reply in thread"
          onClick={() => onOpenThread?.(message)}
        >
          <Reply size={13} />
        </button>
      )}
      {moreItems.length > 0 && (
        <div ref={menu.hostRef} className="chat-bar-wrap">
          <button
            ref={moreBtn}
            type="button"
            className={`chat-bar-btn${more ? " on" : ""}`}
            aria-label="More actions"
            aria-haspopup="menu"
            aria-expanded={more}
            onClick={() => setMore((v) => !v)}
          >
            <MoreHorizontal size={13} />
          </button>
          {menu.portal(
            moreItems.map((it, i) => (
              <button
                key={it.key}
                type="button"
                role="menuitem"
                autoFocus={i === 0}
                className={`chat-more-item${it.danger ? " danger" : ""}`}
                aria-label={it.aria}
                onClick={() => {
                  setMore(false);
                  it.run();
                }}
              >
                {it.icon}
                {it.label}
              </button>
            )),
            "chat-more-menu",
            { role: "menu", onKeyDown: dismissOn(closeMore, moreBtn) },
          )}
        </div>
      )}
    </div>
  );
}

export function ChatView({
  messages,
  onSend,
  onLoadOlder,
  hasMore = false,
  loadingOlder = false,
  currentUserId,
  disabled = false,
  placeholder = "Message…",
  emptyLabel = "No messages yet.",
  onRetry,
  onOpenThread,
  onToggleReaction,
  onEditMessage,
  onDeleteMessage,
  canDeleteAny = false,
  typing,
  beforeComposer,
  hideComposer = false,
}: ChatViewProps) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const [draft, setDraft] = useState("");
  // Which message's body is being edited inline (MAIN-116 AC-3), and the
  // in-progress draft. Each row's popups own their own open state — see
  // `MessageActions`.
  const [editing, setEditing] = useState<{ id: string; draft: string } | null>(null);

  const beginEdit = useCallback((m: ChatViewMessage) => {
    setEditing({ id: m.id, draft: m.body });
  }, []);
  const cancelEdit = useCallback(() => setEditing(null), []);
  const commitEdit = useCallback(
    (original: ChatViewMessage) => {
      if (!editing) return;
      const next = editing.draft.trim();
      setEditing(null);
      // An empty or unchanged edit is a no-op — treat it as a cancel.
      if (next && next !== original.body) onEditMessage?.(original.id, next);
    },
    [editing, onEditMessage],
  );

  // Scroll bookkeeping. `nearBottom` tracks whether appends should follow;
  // `prependRef` records a pending older-page load so its layout effect can
  // restore the exact position instead of letting the content jump.
  const nearBottomRef = useRef(true);
  const prependRef = useRef<{ prevHeight: number; firstId: string } | null>(null);

  const onScroll = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    nearBottomRef.current =
      el.scrollHeight - el.scrollTop - el.clientHeight < NEAR_BOTTOM_PX;
    if (
      el.scrollTop < TOP_TRIGGER_PX &&
      hasMore &&
      !loadingOlder &&
      onLoadOlder &&
      !prependRef.current
    ) {
      prependRef.current = { prevHeight: el.scrollHeight, firstId: messages[0]?.id };
      onLoadOlder();
    }
  }, [hasMore, loadingOlder, onLoadOlder, messages]);

  // After messages change, keep the viewport sensible: restore position across
  // a prepend, follow the bottom on an append when the viewer was already there.
  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const newFirstId = messages[0]?.id;
    const pend = prependRef.current;
    if (pend && newFirstId !== pend.firstId) {
      // Older page arrived on top — shift down by exactly the height it added.
      el.scrollTop = el.scrollHeight - pend.prevHeight + el.scrollTop;
      prependRef.current = null;
    } else if (nearBottomRef.current) {
      el.scrollTop = el.scrollHeight;
    }
  }, [messages, typing]);

  const submit = useCallback(() => {
    const body = draft.trim();
    if (!body || disabled) return;
    onSend(body);
    setDraft("");
  }, [draft, disabled, onSend]);

  const onKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        submit();
      }
    },
    [submit],
  );

  return (
    <div className="chat-view">
      <div className="chat-log" ref={scrollRef} onScroll={onScroll} role="log" aria-live="polite">
        {hasMore && (
          <div className="chat-older">
            {loadingOlder ? "Loading older…" : "Scroll up for older messages"}
          </div>
        )}
        {messages.length === 0 ? (
          <div className="chat-empty">{emptyLabel}</div>
        ) : (
          messages.map((m, i) => {
            const head = startsGroup(m, messages[i - 1]);
            const mine = currentUserId != null && m.authorId === currentUserId;
            // Reactions and edit/delete/react actions only apply to a settled,
            // non-deleted message. A deleted one shows only its placeholder.
            const settled = !m.pending && !m.failed && !m.deleted;
            const canReact = settled && !!onToggleReaction;
            // Edit is always author-only. Delete is author OR tenant admin
            // (MAIN-116 AC-4) — `canDeleteAny` plumbs the admin role, so an admin
            // can remove someone else's message, which the backend already allows.
            const canEdit = settled && mine && !!onEditMessage;
            const canDelete = settled && (mine || canDeleteAny) && !!onDeleteMessage;
            // Replying moved into the hover bar (MAIN-300); the "N replies"
            // affordance below stays, because it reports state rather than
            // offering an action — including on a deleted parent, which is still
            // the only way into a thread hanging off it.
            const canReply = settled && !!onOpenThread;
            // Copy needs no caller: the body is already on screen, so this is
            // offered wherever there is one, read-only surfaces included.
            const canCopy = settled && m.body.trim().length > 0;
            const isEditing = editing?.id === m.id;
            const reactions = m.reactions ?? [];
            return (
              <div
                key={m.id}
                className={`chat-msg${head ? " head" : ""}${mine ? " mine" : ""}${
                  m.pending ? " pending" : ""
                }${m.failed ? " failed" : ""}${m.deleted ? " deleted" : ""}`}
              >
                {head && (
                  <div className="chat-msg-head">
                    <span className="chat-author">{authorLabel(m)}</span>
                    <span className="chat-time">{timeLabel(m.createdAt)}</span>
                  </div>
                )}
                {m.deleted ? (
                  <div className="chat-body deleted">message deleted</div>
                ) : isEditing ? (
                  <textarea
                    className="chat-edit-input"
                    aria-label="Edit message"
                    autoFocus
                    rows={1}
                    value={editing.draft}
                    onChange={(e) => setEditing({ id: m.id, draft: e.target.value })}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" && !e.shiftKey) {
                        e.preventDefault();
                        commitEdit(m);
                      } else if (e.key === "Escape") {
                        e.preventDefault();
                        cancelEdit();
                      }
                    }}
                  />
                ) : (
                  <div className="chat-body">
                    {m.body}
                    {m.edited && <span className="chat-edited"> (edited)</span>}
                  </div>
                )}
                {m.failed && (
                  <button
                    type="button"
                    className="chat-retry"
                    onClick={() => onRetry?.(m)}
                  >
                    Failed — retry
                  </button>
                )}
                {!m.deleted && reactions.length > 0 && (
                  <div className="chat-reactions">
                    {reactions.map((r) => (
                      <button
                        key={r.emoji}
                        type="button"
                        className={`chat-reaction${r.reacted ? " on" : ""}`}
                        aria-pressed={r.reacted}
                        aria-label={`${r.emoji} ${r.count}${
                          r.reacted ? ", remove your reaction" : ""
                        }`}
                        disabled={!onToggleReaction}
                        onClick={() => onToggleReaction?.(m.id, r.emoji, !r.reacted)}
                      >
                        <span className="chat-reaction-emoji">{r.emoji}</span>
                        <span className="chat-reaction-count">{r.count}</span>
                      </button>
                    ))}
                  </div>
                )}
                {!isEditing &&
                  (canReact || canReply || canEdit || canDelete || canCopy) && (
                    <MessageActions
                      message={m}
                      canReact={canReact}
                      canReply={canReply}
                      canEdit={canEdit}
                      canDelete={canDelete}
                      canCopy={canCopy}
                      onToggleReaction={onToggleReaction}
                      onBeginEdit={beginEdit}
                      onDeleteMessage={onDeleteMessage}
                      onOpenThread={onOpenThread}
                    />
                  )}
                {onOpenThread &&
                  !m.pending &&
                  !m.failed &&
                  (m.replyCount ?? 0) > 0 && (
                    <div className="chat-msg-thread">
                      <button
                        type="button"
                        className="chat-thread-count"
                        onClick={() => onOpenThread(m)}
                      >
                        {m.replyCount} {m.replyCount === 1 ? "reply" : "replies"}
                      </button>
                    </div>
                  )}
              </div>
            );
          })
        )}
        {typing && (
          <div className="chat-typing" role="status" aria-live="polite">
            <span className="chat-typing-dots" aria-hidden="true">
              <i />
              <i />
              <i />
            </span>
            {typing}
          </div>
        )}
      </div>
      {beforeComposer}
      {!hideComposer && (
      <div className="chat-composer">
        <textarea
          className="chat-input"
          value={draft}
          disabled={disabled}
          placeholder={placeholder}
          rows={1}
          aria-label="Message"
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={onKeyDown}
        />
        <button
          type="button"
          className="chat-send"
          title="Send message"
          disabled={disabled || draft.trim().length === 0}
          onClick={submit}
        >
          Send
        </button>
      </div>
      )}
    </div>
  );
}
