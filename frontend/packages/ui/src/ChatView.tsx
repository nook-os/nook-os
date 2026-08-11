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

import React, {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import {
  ChevronDown,
  ChevronRight,
  Copy,
  MoreHorizontal,
  Pencil,
  Reply,
  SmilePlus,
  Trash2,
} from "lucide-react";
import { ALLOWED_REACTIONS } from "@nookos/api";
import { EmojiPicker } from "./EmojiPicker";
import { GifPicker, giphyGifUrl } from "./GifPicker";
import { Markdown } from "./Markdown";
import { useAnchoredMenu } from "./useAnchoredMenu";

/** One emoji's tally on a message: how many reacted and whether the viewer is
 *  one of them (so a click toggles it off). Structurally the chat service's
 *  `ChatReactionAggregate`, but named locally to keep the view backend-agnostic. */
export interface ChatViewReaction {
  emoji: string;
  count: number;
  reacted: boolean;
}

/** One command the SERVER offers in this conversation (MAIN-529 AC-1).
 *
 *  DATA ONLY: a name, how its argument reads, and what it does. No callback, no
 *  handler, nothing this component could run — which is what keeps the browser
 *  from ever holding a command set of its own. Structurally the chat service's
 *  `ChatCommand`, named locally like every other shape here so the view stays
 *  backend-agnostic. */
export interface ChatViewCommand {
  /** Without the slash: `help`, not the typed form. */
  name: string;
  /** How the argument reads in the palette, e.g. `<text>` or `[text]`. */
  args_hint?: string | null;
  description: string;
}

/** What running a command answered with — the server's `ChatCommandResult`.
 *  `ephemeral` is text for the invoker's eyes only; `posted_message_id` names a
 *  message the command posted, which arrives by the ordinary live path. */
export interface ChatViewCommandResult {
  ephemeral?: string | null;
  posted_message_id?: string | null;
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
  /** How to render the body.
   *
   *  - absent/false — plain pre-wrapped text, exactly as typed.
   *  - `true` — a markdown DOCUMENT (MAIN-299): a loop run's drafted issue is a
   *    whole spec, unreadable as a wall of literal `##`. CommonMark newline
   *    rules apply, so a soft-wrapped paragraph stays one paragraph.
   *  - `"chat"` — markdown with chat's newline semantics: a single newline is a
   *    line break, because the person pressed Shift+Enter meaning "go down a
   *    line", not "continue this paragraph".
   *
   *  The caller decides — the view has no way to tell prose from a draft, and
   *  guessing would eventually render someone's message wrong. */
  markdown?: boolean | "chat";
  /** This message is an ACTION rather than something its author said (MAIN-529
   *  AC-8) — what the chat service marks `kind = "action"`. It renders italic
   *  and author-prefixed on one line, carries no reaction row and offers no
   *  Edit; deleting it is exactly as ordinary. The caller decides: the view
   *  never infers an action from a body. */
  action?: boolean;
  /** This message is folded TOOL ACTIVITY rather than something its author said
   *  (MAIN-499): the steps it stands for, in order, as they were recorded.
   *
   *  Presence is the discriminator — at `variant="transcript"` such a message
   *  renders as its own kind, expandable to these lines, so "what it said" and
   *  "what it did" are told apart at a glance. The folding is the caller's job
   *  (`foldToolActivity`); this view only refuses to throw the detail away. */
  activity?: string[];
}

/** How the list READS.
 *
 *  - `"chat"` — the default, and team chat's tuned density. Unchanged.
 *  - `"transcript"` — an agent run: consecutive turns from one author share a
 *    header however far apart they are, turns are separated vertically, and a
 *    message carrying `activity` renders as tool activity (MAIN-499). */
export type ChatViewVariant = "chat" | "transcript";

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
  /** The submit button's label. Defaults to "Send"; the loop reuses this box to
   *  START a run, where "Draft a spec" / "Decompose" reads truer than "Send". */
  sendLabel?: string;
  /** Allow submitting an empty box. Off by default — an empty steer or reply is
   *  never meaningful. The loop's SEED mode turns it on: sending an empty seed
   *  is a real action ("start the run, read the ticket alone"). */
  allowEmpty?: boolean;
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
  /** The viewer is actively composing — fired on every keystroke that leaves
   *  text in the box (MAIN-163 AC-2). The caller owns the throttle and the
   *  network: this component only reports the activity, which is what keeps it
   *  backend-agnostic. Clearing the box fires nothing; there is no stop signal
   *  to send. */
  onTypingActivity?: () => void;
  /** Rendered between the log and the composer — where a caller puts controls
   *  that belong to the reply rather than to the conversation (the loop's
   *  interaction choice buttons). Team chat passes nothing and is unchanged. */
  beforeComposer?: React.ReactNode;
  /** Hide the composer entirely, for a surface that is read-only right now (a
   *  finished loop job). Distinct from `disabled`, which shows the box greyed:
   *  when there is nothing to say TO, an inert box is just clutter. */
  hideComposer?: boolean;
  /** The deployment's Giphy API key (MAIN-171 AC-2), from `/api/v1/config`.
   *
   *  Absent or null — the shipped state of any deployment whose operator has
   *  not brought a key — renders NO GIF button at all (AC-3): not a disabled
   *  one, not one that fails when pressed. The emoji picker is unaffected; it
   *  needs no service. Callers that are not team chat (the loop's run view)
   *  simply pass nothing. */
  giphyKey?: string | null;
  /** Which reading this list gets (MAIN-499). Omitted is `"chat"`, byte for
   *  byte what every consumer rendered before the variant existed. */
  variant?: ChatViewVariant;
  /** The commands the server offers here (MAIN-529 AC-1), for the palette a
   *  leading slash opens. Absent — every surface that has not wired the two
   *  endpoints — means no palette and no command parsing at all: typed text is
   *  passed to `onSend` exactly as it always was. */
  commands?: ChatViewCommand[];
  /** Run one of `commands` as the caller, answering with what the server said
   *  (AC-1). The view posts a name and the remaining text and renders the
   *  result; it never learns what a command means, and a rejected promise
   *  renders through the same path as an `ephemeral` (AC-7). */
  onCommand?: (
    name: string,
    args: string,
  ) => Promise<ChatViewCommandResult> | ChatViewCommandResult;
  /** An opaque identity for the conversation on screen. The view never reads
   *  it: a CHANGE is the signal that drops the ephemeral notes, which belong to
   *  the conversation they were answered in and must not follow the reader into
   *  the next one (AC-7). A surface with one conversation passes nothing. */
  conversationId?: string;
}

/**
 * Splice `insert` over the range `[start, end)` and report where the caret ends
 * up — the whole of "insert at the cursor" (AC-1), as a pure function so the
 * behaviour is testable without a DOM selection.
 *
 * `end` handles the selection case: typing an emoji over selected text replaces
 * it, which is what every editor does.
 */
export function insertAt(
  text: string,
  start: number,
  end: number,
  insert: string,
): { text: string; caret: number } {
  const from = Math.max(0, Math.min(start, text.length));
  const to = Math.max(from, Math.min(end, text.length));
  return {
    text: text.slice(0, from) + insert + text.slice(to),
    caret: from + insert.length,
  };
}

/**
 * What the palette is filtering on, or `null` when the composer is not a command
 * line at all (AC-3).
 *
 * A slash ONLY opens the palette as the first character, and only until the
 * name is settled: once whitespace has been typed the person is writing
 * arguments, and a palette still open there would swallow the Enter that runs
 * the command (AC-4).
 */
export function paletteQuery(text: string): string | null {
  if (!text.startsWith("/")) return null;
  const rest = text.slice(1);
  return /\s/.test(rest) ? null : rest;
}

/** The entries a query offers (AC-3). A prefix match on the name, in the
 *  server's order — the view neither ranks nor rewrites the list it was given. */
export function matchCommands(
  commands: ChatViewCommand[],
  query: string,
): ChatViewCommand[] {
  const q = query.toLowerCase();
  return commands.filter((c) => c.name.toLowerCase().startsWith(q));
}

/**
 * Read submitted text as a command invocation, or `null` when it is an ordinary
 * message (AC-5/AC-6).
 *
 * The leading token has to be a name the SERVER listed — matched exactly, as the
 * server matches it. Leading-slash text that is not in the list is not a
 * command and is never treated as one: that is how `/nook-spec …` still reaches
 * an agent verbatim on the surfaces that pass no commands, and on the ones that
 * do.
 */
export function parseCommand(
  text: string,
  commands: ChatViewCommand[],
): { name: string; args: string } | null {
  if (!text.startsWith("/")) return null;
  const rest = text.slice(1);
  const gap = rest.search(/\s/);
  const name = gap < 0 ? rest : rest.slice(0, gap);
  if (!commands.some((c) => c.name === name)) return null;
  return { name, args: gap < 0 ? "" : rest.slice(gap + 1) };
}

/** The palette is empty until something is typed, so it is sized for a handful
 *  of rows — enough for `useAnchoredMenu` to flip it above the composer, which
 *  is where it always belongs. */
const PALETTE_HEIGHT = 220;

/** A stable empty list, so a surface passing no commands does not hand the
 *  memoised filter a fresh array on every render. */
const NO_COMMANDS: ChatViewCommand[] = [];

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

/** First of a run from one author within the grouping window → show a header.
 *
 *  `windowed` is what the transcript variant drops (MAIN-499 AC-2): a chat
 *  message five minutes later is a new thought and deserves a fresh header, but
 *  an agent's turns are minutes apart BY NATURE, so the window put a repeated
 *  "agent · 07:54 PM" over every single one. Consecutive turns from one author
 *  are one run there, whatever the clock says. */
function startsGroup(
  m: ChatViewMessage,
  prev: ChatViewMessage | undefined,
  windowed: boolean,
): boolean {
  if (!prev) return true;
  // An action carries its own author inline and shows no header (AC-8), so the
  // message after one has nothing above it saying who is speaking.
  if (prev.action) return true;
  if (prev.authorId !== m.authorId) return true;
  if (!windowed) return false;
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
  time,
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
  /** Shown at the bar's left edge on grouped rows, which have no header of
   *  their own — hovering is the only way to date one. Head rows pass nothing:
   *  their header already says it. */
  time?: string;
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
      {time && <span className="chat-bar-time">{time}</span>}
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

/**
 * A folded run of tool calls, as its own kind (MAIN-499 AC-4/AC-5).
 *
 * Dim, monospace and set apart, because `· 37 steps — Bash ×37` is not prose
 * and reading it as prose is what made a long transcript a wall. The steps the
 * line stands for are one click away rather than gone — exporting the whole
 * transcript used to be the only way to see them.
 *
 * A line with no retained steps is not a button: an affordance that opens onto
 * nothing is worse than no affordance.
 */
function ToolActivity({
  label,
  steps,
  open,
  onToggle,
}: {
  label: string;
  steps: string[];
  open: boolean;
  onToggle: () => void;
}) {
  return (
    <div className="chat-tool">
      {steps.length === 0 ? (
        <span className="chat-tool-line">{label}</span>
      ) : (
        <button
          type="button"
          className="chat-tool-line"
          aria-expanded={open}
          onClick={onToggle}
        >
          {open ? <ChevronDown size={11} /> : <ChevronRight size={11} />}
          <span>{label}</span>
        </button>
      )}
      {open && steps.length > 0 && (
        <ul className="chat-tool-steps">
          {steps.map((s, i) => (
            <li key={`${i}-${s}`}>{s}</li>
          ))}
        </ul>
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
  sendLabel = "Send",
  allowEmpty = false,
  emptyLabel = "No messages yet.",
  onRetry,
  onOpenThread,
  onToggleReaction,
  onEditMessage,
  onDeleteMessage,
  canDeleteAny = false,
  typing,
  onTypingActivity,
  beforeComposer,
  hideComposer = false,
  giphyKey,
  variant = "chat",
  commands,
  onCommand,
  conversationId,
}: ChatViewProps) {
  const transcript = variant === "transcript";
  const scrollRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const [draft, setDraft] = useState("");
  // Which folded activity lines the reader has opened, by message id. Keyed
  // rather than a single open row: reading a transcript means comparing two
  // runs of steps, not being shown one at a time.
  const [openActivity, setOpenActivity] = useState<Record<string, boolean>>({});
  const toggleActivity = useCallback((id: string) => {
    setOpenActivity((prev) => ({ ...prev, [id]: !prev[id] }));
  }, []);
  // Which message's body is being edited inline (MAIN-116 AC-3), and the
  // in-progress draft. Each row's popups own their own open state — see
  // `MessageActions`.
  const [editing, setEditing] = useState<{ id: string; draft: string } | null>(null);
  // What commands answered (AC-7). Client-only and unsent: a note is this
  // reader's copy of what the server told them, and nothing here ever posts it.
  const [notes, setNotes] = useState<{ id: string; text: string }[]>([]);
  const noteCounter = useRef(0);
  // Dismissal, for the query that was on screen when it happened (AC-4) — set
  // by Escape and by an outside click, cleared the moment the query changes.
  // A flag rather than an edit to the draft, because the draft IS the query:
  // the next keystroke asks again, which is what a dismissed menu should do.
  // Nothing else may clear it; a second clearer is what let one dismissal
  // strand the palette for the rest of the composing session.
  const [paletteOff, setPaletteOff] = useState(false);
  const [cmdIndex, setCmdIndex] = useState(0);
  const addNote = useCallback((text: string) => {
    setNotes((prev) => [...prev, { id: `note-${noteCounter.current++}`, text }]);
  }, []);
  // A new conversation answers its own commands; the last one's replies are not
  // part of it. Nothing persists them, so a reload starts empty for free.
  useEffect(() => setNotes([]), [conversationId]);

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
  }, [messages, typing, notes]);

  const commandSet = commands ?? NO_COMMANDS;

  /** Post a command and show whatever comes back (AC-7). A refusal is the server
   *  ANSWERING what was typed, so it renders where the typing happened rather
   *  than as a toast or a wall of JSON — the same path an `ephemeral` takes. */
  const invoke = useCallback(
    async (name: string, args: string) => {
      if (!onCommand) return;
      try {
        const res = await onCommand(name, args);
        if (res?.ephemeral) addNote(res.ephemeral);
      } catch (err) {
        addNote(err instanceof Error ? err.message : String(err));
      }
    },
    [onCommand, addNote],
  );

  const submit = useCallback(() => {
    const body = draft.trim();
    if (disabled || (!body && !allowEmpty)) return;
    // A leading token the SERVER listed is a command; anything else — including
    // leading-slash text matching nothing — is the message it looks like
    // (AC-5/AC-6).
    const cmd = onCommand ? parseCommand(body, commandSet) : null;
    if (cmd) void invoke(cmd.name, cmd.args);
    else onSend(body);
    setDraft("");
    // Collapse back to the one-line resting height the stylesheet pins.
    const el = inputRef.current;
    if (el) el.style.height = "";
  }, [draft, disabled, allowEmpty, onSend, onCommand, commandSet, invoke]);

  /** Grow the box with its content instead of scrolling inside a 34px slot.
   *  Inline style, cleared at rest, so the stylesheet keeps owning the resting
   *  height and the Send button stays matched to it. The max mirrors the
   *  stylesheet's `max-height` — past that the box scrolls, as it should. */
  const autoGrow = useCallback(() => {
    const el = inputRef.current;
    if (!el) return;
    el.style.height = "";
    if (el.scrollHeight > el.clientHeight) {
      el.style.height = `${Math.min(el.scrollHeight + 2, 160)}px`;
    }
  }, []);

  /** Put a character where the caret is (AC-1), then leave the caret after it
   *  and the focus in the box — so picking an emoji is a step in composing a
   *  message rather than the end of one.
   *
   *  The caret has to be restored in an effect-like step after React has
   *  re-rendered with the new value; setting `selectionStart` before that would
   *  be overwritten by the re-render, which is the bug that puts the caret at
   *  the end of the box. */
  const insertEmoji = useCallback(
    (emoji: string) => {
      const el = inputRef.current;
      const at = el?.selectionStart ?? draft.length;
      const to = el?.selectionEnd ?? at;
      const next = insertAt(draft, at, to, emoji);
      setDraft(next.text);
      requestAnimationFrame(() => {
        const box = inputRef.current;
        if (!box) return;
        box.focus();
        box.setSelectionRange(next.caret, next.caret);
        autoGrow();
      });
    },
    [draft, autoGrow],
  );

  const query = onCommand ? paletteQuery(draft) : null;
  const matches = useMemo(
    () => (query === null ? [] : matchCommands(commandSet, query)),
    [commandSet, query],
  );
  const paletteOpen = !paletteOff && matches.length > 0;
  const selected = Math.min(cmdIndex, matches.length - 1);
  // A different query is a different question: it gets the first row
  // highlighted rather than wherever the last list was left, and it undoes a
  // dismissal, which belonged to the query it was typed against.
  useEffect(() => {
    setCmdIndex(0);
    setPaletteOff(false);
  }, [query]);

  const closePalette = useCallback(() => setPaletteOff(true), []);
  const palette = useAnchoredMenu(paletteOpen, closePalette, {
    height: PALETTE_HEIGHT,
    matchWidth: true,
  });

  /** Put the highlighted command in the box with a space after it, ready for its
   *  arguments (AC-4). Completing is not running: the person presses Enter once
   *  more, having seen what they are about to send. */
  const completeCommand = useCallback((name: string) => {
    const next = `/${name} `;
    setDraft(next);
    requestAnimationFrame(() => {
      const box = inputRef.current;
      if (!box) return;
      box.focus();
      box.setSelectionRange(next.length, next.length);
    });
  }, []);

  const onKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
      if (paletteOpen) {
        if (e.key === "ArrowDown") {
          e.preventDefault();
          setCmdIndex((i) => (Math.min(i, matches.length - 1) + 1) % matches.length);
          return;
        }
        if (e.key === "ArrowUp") {
          e.preventDefault();
          setCmdIndex(
            (i) =>
              (Math.min(i, matches.length - 1) + matches.length - 1) % matches.length,
          );
          return;
        }
        if (e.key === "Tab" || (e.key === "Enter" && !e.shiftKey)) {
          // AC-4: with the palette open Enter completes and never sends.
          e.preventDefault();
          completeCommand(matches[selected].name);
          return;
        }
        if (e.key === "Escape") {
          e.preventDefault();
          setPaletteOff(true);
          return;
        }
      }
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        submit();
      }
    },
    [paletteOpen, matches, selected, completeCommand, submit],
  );

  return (
    <div className="chat-view">
      <div
        className={`chat-log${transcript ? " transcript" : ""}`}
        ref={scrollRef}
        onScroll={onScroll}
        role="log"
        aria-live="polite"
      >
        {hasMore && (
          <div className="chat-older">
            {loadingOlder ? "Loading older…" : "Scroll up for older messages"}
          </div>
        )}
        {messages.length === 0 ? (
          <div className="chat-empty">{emptyLabel}</div>
        ) : (
          messages.map((m, i) => {
            const head = startsGroup(m, messages[i - 1], !transcript);
            // Only the transcript has a "what it did" kind to separate from
            // "what it said"; in chat every message is prose (AC-1).
            const activity = transcript ? m.activity : undefined;
            const mine = currentUserId != null && m.authorId === currentUserId;
            // An action is a stage direction, not a remark (AC-8): it says who
            // did the thing inside its own line, so it shows no header, and it
            // is not something to react to or to reword afterwards. Deleting it
            // is exactly as ordinary.
            const action = !m.deleted && !!m.action;
            // Reactions and edit/delete/react actions only apply to a settled,
            // non-deleted message. A deleted one shows only its placeholder.
            const settled = !m.pending && !m.failed && !m.deleted;
            const canReact = settled && !action && !!onToggleReaction;
            // Edit is always author-only. Delete is author OR tenant admin
            // (MAIN-116 AC-4) — `canDeleteAny` plumbs the admin role, so an admin
            // can remove someone else's message, which the backend already allows.
            const canEdit = settled && !action && mine && !!onEditMessage;
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
            // A message that IS a Giphy URL renders as the picture (MAIN-171
            // AC-2). Recognised from the body, not from a flag, so a GIF posted
            // before this shipped renders too — and so nothing but a giphy.com
            // image URL can ever become an `<img>`.
            const gif = m.deleted ? null : giphyGifUrl(m.body);
            return (
              <div
                key={m.id}
                data-author={m.authorId}
                data-kind={activity ? "activity" : action ? "action" : undefined}
                className={`chat-msg${head ? " head" : ""}${mine ? " mine" : ""}${
                  m.pending ? " pending" : ""
                }${m.failed ? " failed" : ""}${m.deleted ? " deleted" : ""}`}
              >
                {head && !action && (
                  <div className="chat-msg-head">
                    <span className="chat-author">{authorLabel(m)}</span>
                    <span className="chat-time">{timeLabel(m.createdAt)}</span>
                  </div>
                )}
                {m.deleted ? (
                  <div className="chat-body deleted">message deleted</div>
                ) : activity ? (
                  <ToolActivity
                    label={m.body}
                    steps={activity}
                    open={!!openActivity[m.id]}
                    onToggle={() => toggleActivity(m.id)}
                  />
                ) : action ? (
                  // AC-8: one italic line, author first — `<em>` rather than a
                  // stylesheet rule, so the emphasis is in the document a
                  // screen reader reads and not only in the paint.
                  <div className="chat-body action">
                    <em>
                      {authorLabel(m)} {m.body}
                    </em>
                  </div>
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
                ) : gif ? (
                  // A GIF message is its URL and nothing else (AC-2), so it
                  // renders as the picture rather than as the link — bounded by
                  // the stylesheet, and opening the full-size original in a new
                  // tab when clicked. Checked BEFORE the markdown path: a bare
                  // URL is valid markdown, and rendering it as an autolink is
                  // exactly what this replaces.
                  <div className="chat-body gif">
                    <a href={gif} target="_blank" rel="noreferrer noopener">
                      <img className="chat-gif" src={gif} alt="GIF" />
                    </a>
                    {/* AC-4: GIPHY's attribution mark travels with the GIF, not
                        only with the picker it was chosen from. */}
                    <a
                      className="chat-gif-attribution"
                      href="https://giphy.com"
                      target="_blank"
                      rel="noreferrer noopener"
                    >
                      via GIPHY
                    </a>
                  </div>
                ) : (
                  <div className={`chat-body${m.markdown ? " md" : ""}`}>
                    {m.markdown ? (
                      <Markdown src={m.body} breaks={m.markdown === "chat"} />
                    ) : (
                      m.body
                    )}
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
                {!m.deleted && !action && reactions.length > 0 && (
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
                      time={head && !action ? undefined : timeLabel(m.createdAt)}
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
        {notes.map((n) => (
          <div key={n.id} className="chat-note" role="status">
            {n.text}
          </div>
        ))}
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
      <div className="chat-composer" ref={palette.hostRef}>
        <textarea
          ref={inputRef}
          className="chat-input"
          value={draft}
          disabled={disabled}
          placeholder={placeholder}
          rows={1}
          aria-label="Message"
          {...(paletteOpen
            ? {
                role: "combobox",
                "aria-expanded": true,
                "aria-controls": "chat-cmd-palette",
                "aria-activedescendant": `chat-cmd-${matches[selected].name}`,
              }
            : {})}
          onChange={(e) => {
            setDraft(e.target.value);
            autoGrow();
            if (e.target.value.trim().length > 0) onTypingActivity?.();
          }}
          onKeyDown={onKeyDown}
        />
        {/* The pickers sit AFTER the textarea in the DOM and are pulled back to
            the left of it by `.chat-composer-wrap { order: -1 }`, so tabbing
            into the composer lands on the message box — the primary control —
            rather than on a picker. Visual order is unchanged. */}
        <EmojiPicker onPick={insertEmoji} disabled={disabled} />
        {/* AC-3: no key, no button. Not a disabled one — an affordance that is
            always there and never works is worse than one that is not. */}
        {giphyKey && (
          <GifPicker apiKey={giphyKey} onPick={onSend} disabled={disabled} />
        )}
        <button
          type="button"
          className="chat-send"
          title="Send message"
          disabled={disabled || (draft.trim().length === 0 && !allowEmpty)}
          onClick={submit}
        >
          {sendLabel}
        </button>
        {/* The list the server gave us, filtered — and nothing else. Portalled
            out of the composer by `useAnchoredMenu` for the same reason the
            reaction and more menus are: the panel around it has an overflow
            that would clip it. */}
        {palette.portal(
          matches.map((c, i) => (
            <button
              key={c.name}
              id={`chat-cmd-${c.name}`}
              type="button"
              role="option"
              aria-selected={i === selected}
              className={`chat-cmd-option${i === selected ? " on" : ""}`}
              // The box keeps focus the whole time the palette is open, so a
              // press here must not take it away — the click still completes.
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => completeCommand(c.name)}
            >
              <span className="chat-cmd-name">/{c.name}</span>
              {c.args_hint && <span className="chat-cmd-args">{c.args_hint}</span>}
              <span className="chat-cmd-desc">{c.description}</span>
            </button>
          )),
          "chat-cmd-palette",
          { id: "chat-cmd-palette", role: "listbox", "aria-label": "Commands" },
        )}
      </div>
      )}
    </div>
  );
}
