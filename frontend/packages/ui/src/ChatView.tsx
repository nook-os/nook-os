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
import { SmilePlus } from "lucide-react";
import { ALLOWED_REACTIONS } from "@nookos/api";

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
  /** Open a message's thread (MAIN-114). When set, each confirmed message shows
   *  a "Reply in thread" action, and a "N replies" affordance when it has any.
   *  Omit it — as the thread panel's own reply list does — to render a plain
   *  list with no per-message thread actions (no nesting, NG-1). */
  onOpenThread?: (message: ChatViewMessage) => void;
  /** Toggle a reaction (MAIN-116 AC-2). `on` is the desired next state: clicking
   *  an un-highlighted pill or picking from the "add reaction" menu calls it
   *  with `true`; clicking a highlighted pill calls it with `false`. Available
   *  to everyone; omit to render read-only reactions with no picker. */
  onToggleReaction?: (messageId: string, emoji: string, on: boolean) => void;
  /** Save an inline edit (MAIN-116 AC-3). Only offered on the viewer's own,
   *  non-deleted messages; omit to suppress the Edit action entirely. */
  onEditMessage?: (messageId: string, newBody: string) => void;
  /** Delete a message (MAIN-116 AC-4). Only offered on the viewer's own,
   *  non-deleted messages; the caller owns any confirmation. Omit to suppress. */
  onDeleteMessage?: (messageId: string) => void;
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
}: ChatViewProps) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const [draft, setDraft] = useState("");
  // Which message's body is being edited inline (MAIN-116 AC-3), and the
  // in-progress draft; and which message's "add reaction" picker is open.
  const [editing, setEditing] = useState<{ id: string; draft: string } | null>(null);
  const [pickerFor, setPickerFor] = useState<string | null>(null);

  const beginEdit = useCallback((m: ChatViewMessage) => {
    setPickerFor(null);
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
  }, [messages]);

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
            const canEdit = settled && mine && !!onEditMessage;
            const canDelete = settled && mine && !!onDeleteMessage;
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
                {!isEditing && (canReact || canEdit || canDelete) && (
                  <div className="chat-msg-actions">
                    {canReact && (
                      <div className="chat-react-wrap">
                        <button
                          type="button"
                          className={`chat-act chat-act-react${
                            pickerFor === m.id ? " open" : ""
                          }`}
                          aria-label="Add reaction"
                          aria-expanded={pickerFor === m.id}
                          onClick={() =>
                            setPickerFor((cur) => (cur === m.id ? null : m.id))
                          }
                        >
                          <SmilePlus size={13} />
                        </button>
                        {pickerFor === m.id && (
                          <div className="chat-react-picker" role="menu">
                            {ALLOWED_REACTIONS.map((emoji) => (
                              <button
                                key={emoji}
                                type="button"
                                className="chat-react-opt"
                                role="menuitem"
                                aria-label={`React with ${emoji}`}
                                onClick={() => {
                                  setPickerFor(null);
                                  onToggleReaction?.(m.id, emoji, true);
                                }}
                              >
                                {emoji}
                              </button>
                            ))}
                          </div>
                        )}
                      </div>
                    )}
                    {canEdit && (
                      <button
                        type="button"
                        className="chat-act"
                        aria-label="Edit message"
                        onClick={() => beginEdit(m)}
                      >
                        Edit
                      </button>
                    )}
                    {canDelete && (
                      <button
                        type="button"
                        className="chat-act chat-act-danger"
                        aria-label="Delete message"
                        onClick={() => onDeleteMessage?.(m.id)}
                      >
                        Delete
                      </button>
                    )}
                  </div>
                )}
                {onOpenThread && !m.pending && !m.failed && (
                  <div className="chat-msg-thread">
                    {m.replyCount && m.replyCount > 0 ? (
                      <button
                        type="button"
                        className="chat-thread-count"
                        onClick={() => onOpenThread(m)}
                      >
                        {m.replyCount} {m.replyCount === 1 ? "reply" : "replies"}
                      </button>
                    ) : (
                      <button
                        type="button"
                        className="chat-thread-reply"
                        onClick={() => onOpenThread(m)}
                        aria-label="Reply in thread"
                      >
                        Reply in thread
                      </button>
                    )}
                  </div>
                )}
              </div>
            );
          })
        )}
      </div>
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
          disabled={disabled || draft.trim().length === 0}
          onClick={submit}
        >
          Send
        </button>
      </div>
    </div>
  );
}
