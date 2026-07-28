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
}: ChatViewProps) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const [draft, setDraft] = useState("");

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
            return (
              <div
                key={m.id}
                className={`chat-msg${head ? " head" : ""}${mine ? " mine" : ""}${
                  m.pending ? " pending" : ""
                }${m.failed ? " failed" : ""}`}
              >
                {head && (
                  <div className="chat-msg-head">
                    <span className="chat-author">{authorLabel(m)}</span>
                    <span className="chat-time">{timeLabel(m.createdAt)}</span>
                  </div>
                )}
                <div className="chat-body">{m.body}</div>
                {m.failed && (
                  <button
                    type="button"
                    className="chat-retry"
                    onClick={() => onRetry?.(m)}
                  >
                    Failed — retry
                  </button>
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
