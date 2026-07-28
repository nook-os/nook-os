// AC-4: an optimistic post reconciles with its server echo — no double render.
// This tests the merge logic directly (pure, no DOM/WS), which is where the
// dedupe actually lives.
import { describe, expect, it } from "vitest";
import type { ChatMessage } from "@nookos/api";
import {
  applyMessageUpdate,
  buildChatMessages,
  buildThreadMessages,
  reconcilePending,
  type PendingMessage,
} from "./chatMessages";

type Reaction = { emoji: string; count: number; reacted: boolean };
const withReactions = (m: ChatMessage, reactions: Reaction[]): ChatMessage => ({
  ...m,
  reactions,
});

const msg = (id: string, author: string, body: string, t: string): ChatMessage => ({
  id,
  author_id: author,
  channel_id: "c1",
  body,
  created_at: t,
});

const pend = (tempId: string, author: string, body: string): PendingMessage => ({
  tempId,
  authorId: author,
  body,
  createdAt: "2026-07-25T10:00:00Z",
});

describe("buildChatMessages", () => {
  it("orders confirmed messages oldest → newest and dedupes by id", () => {
    const history = [msg("b", "u1", "two", "2026-07-25T10:00:02Z")];
    const live = [
      msg("a", "u1", "one", "2026-07-25T10:00:01Z"),
      msg("b", "u1", "two", "2026-07-25T10:00:02Z"), // same id as history
    ];
    const out = buildChatMessages(history, live, [], "me");
    expect(out.map((m) => m.id)).toEqual(["a", "b"]);
  });

  it("appends a live message from another user", () => {
    const out = buildChatMessages([], [msg("x", "u2", "hi", "2026-07-25T10:01:00Z")], [], "me");
    expect(out).toHaveLength(1);
    expect(out[0].body).toBe("hi");
  });

  it("drops an optimistic post once its own echo arrives (no double render)", () => {
    const pending = [pend("t1", "me", "ping")];
    // Before the echo: the optimistic bubble shows, marked pending.
    const before = buildChatMessages([], [], pending, "me");
    expect(before).toHaveLength(1);
    expect(before[0].pending).toBe(true);

    // Echo arrives over the WS as a confirmed message from me.
    const echo = [msg("real1", "me", "ping", "2026-07-25T10:00:05Z")];
    const after = buildChatMessages([], echo, pending, "me");
    expect(after).toHaveLength(1);
    expect(after[0].id).toBe("real1");
    expect(after[0].pending).toBeUndefined();
  });

  it("keeps two identical optimistic posts until both echoes land", () => {
    const pending = [pend("t1", "me", "ok"), pend("t2", "me", "ok")];
    const oneEcho = [msg("r1", "me", "ok", "2026-07-25T10:00:05Z")];
    const out = buildChatMessages([], oneEcho, pending, "me");
    // One confirmed + one still-pending "ok".
    expect(out).toHaveLength(2);
    expect(out.filter((m) => m.pending)).toHaveLength(1);
  });

  it("does not let an old identical message cancel a fresh optimistic post", () => {
    // My earlier "ok" is history, not a live echo — it must not cancel a new send.
    const history = [msg("old", "me", "ok", "2026-07-25T09:00:00Z")];
    const pending = [pend("t1", "me", "ok")];
    const out = buildChatMessages(history, [], pending, "me");
    expect(out.filter((m) => m.pending)).toHaveLength(1);
  });

  // ── Threaded replies (MAIN-114) ──
  const reply = (id: string, parent: string, t: string): ChatMessage => ({
    ...msg(id, "u1", "a reply", t),
    parent_message_id: parent,
  });

  it("excludes replies from the channel stream (AC-2)", () => {
    const history = [
      { ...msg("p", "u1", "parent", "2026-07-25T10:00:00Z") },
      reply("r", "p", "2026-07-25T10:00:01Z"),
    ];
    const out = buildChatMessages(history, [], [], "me");
    expect(out.map((m) => m.id)).toEqual(["p"]);
  });

  it("surfaces the server reply_count on a history parent (AC-3)", () => {
    const parent: ChatMessage = {
      ...msg("p", "u1", "parent", "2026-07-25T10:00:00Z"),
      reply_count: 3,
    };
    const out = buildChatMessages([parent], [], [], "me");
    expect(out[0].replyCount).toBe(3);
  });

  it("routes a live reply to a count bump, not the main stream (AC-4)", () => {
    const parent: ChatMessage = {
      ...msg("p", "u1", "parent", "2026-07-25T10:00:00Z"),
      reply_count: 1, // one reply existed at load
    };
    // A new reply arrives live over the channel socket.
    const live = [reply("r-live", "p", "2026-07-25T10:05:00Z")];
    const out = buildChatMessages([parent], live, [], "me");
    // The reply never shows in the stream…
    expect(out.map((m) => m.id)).toEqual(["p"]);
    // …but the parent's count bumps from 1 → 2.
    expect(out[0].replyCount).toBe(2);
  });

  it("leaves a childless message with no reply affordance", () => {
    const out = buildChatMessages([msg("p", "u1", "lonely", "2026-07-25T10:00:00Z")], [], [], "me");
    expect(out[0].replyCount).toBeUndefined();
  });
});

describe("buildThreadMessages", () => {
  const reply = (id: string, parent: string, author: string, body: string, t: string): ChatMessage => ({
    ...msg(id, author, body, t),
    parent_message_id: parent,
  });

  it("keeps only this parent's replies, oldest → newest", () => {
    const replies = [
      reply("r2", "p", "u1", "second", "2026-07-25T10:00:02Z"),
      reply("r1", "p", "u1", "first", "2026-07-25T10:00:01Z"),
    ];
    // The live buffer also holds channel chatter and another thread's reply.
    const live = [
      msg("top", "u1", "channel msg", "2026-07-25T10:00:03Z"),
      reply("other", "q", "u1", "elsewhere", "2026-07-25T10:00:04Z"),
    ];
    const out = buildThreadMessages(replies, live, [], "p", "me");
    expect(out.map((m) => m.id)).toEqual(["r1", "r2"]);
  });

  it("folds a live reply into the thread and reconciles my optimistic reply", () => {
    const pending = [pend("t1", "me", "hello")];
    const before = buildThreadMessages([], [], pending, "p", "me");
    expect(before).toHaveLength(1);
    expect(before[0].pending).toBe(true);

    // My reply echoes back live, tagged to this parent.
    const echo = [reply("r-real", "p", "me", "hello", "2026-07-25T10:00:05Z")];
    const after = buildThreadMessages([], echo, pending, "p", "me");
    expect(after).toHaveLength(1);
    expect(after[0].id).toBe("r-real");
    expect(after[0].pending).toBeUndefined();
  });

  it("never emits a reply affordance inside a thread (no nesting, NG-1)", () => {
    const replies = [reply("r1", "p", "u1", "hi", "2026-07-25T10:00:01Z")];
    const out = buildThreadMessages(replies, [], [], "p", "me");
    expect(out[0].replyCount).toBeUndefined();
  });
});

// MAIN-116: the reactions-merge rule + edited/deleted propagation. A
// `message_updated` broadcast carries viewer-NEUTRAL reactions (reacted always
// false); the client must keep its own `reacted` while taking the new counts.
describe("applyMessageUpdate", () => {
  const base = msg("m1", "u1", "hello", "2026-07-25T10:00:00Z");

  it("takes the incoming counts but preserves the client's own reacted", () => {
    const existing = withReactions(base, [{ emoji: "👍", count: 1, reacted: true }]);
    // Someone else also reacted 👍 → broadcast count is 2, reacted neutral (false).
    const incoming = withReactions(base, [{ emoji: "👍", count: 2, reacted: false }]);
    const out = applyMessageUpdate(existing, incoming);
    expect(out.reactions).toEqual([{ emoji: "👍", count: 2, reacted: true }]);
  });

  it("defaults reacted to false for an emoji the client never had", () => {
    const existing = withReactions(base, [{ emoji: "👍", count: 1, reacted: true }]);
    const incoming = withReactions(base, [
      { emoji: "👍", count: 1, reacted: false },
      { emoji: "🎉", count: 1, reacted: false }, // new emoji from someone else
    ]);
    const out = applyMessageUpdate(existing, incoming);
    expect(out.reactions).toEqual([
      { emoji: "👍", count: 1, reacted: true },
      { emoji: "🎉", count: 1, reacted: false },
    ]);
  });

  it("treats a missing existing as no prior reactions (all reacted false)", () => {
    const incoming = withReactions(base, [{ emoji: "🚀", count: 3, reacted: false }]);
    const out = applyMessageUpdate(undefined, incoming);
    expect(out.reactions).toEqual([{ emoji: "🚀", count: 3, reacted: false }]);
  });

  it("carries the edited and deleted flags and the (possibly redacted) body", () => {
    const edited = { ...base, body: "hello (fixed)", edited_at: "2026-07-25T10:05:00Z" };
    expect(applyMessageUpdate(base, edited).edited_at).toBe("2026-07-25T10:05:00Z");
    expect(applyMessageUpdate(base, edited).body).toBe("hello (fixed)");

    const deleted = { ...base, body: "message deleted", deleted: true, reactions: [] };
    const out = applyMessageUpdate(withReactions(base, [{ emoji: "👍", count: 1, reacted: true }]), deleted);
    expect(out.deleted).toBe(true);
    expect(out.reactions).toEqual([]);
  });

  it("surfaces reactions, the edited marker, and the deleted flag through the view", () => {
    const reacted = withReactions(base, [{ emoji: "❤️", count: 2, reacted: true }]);
    const edited = { ...reacted, edited_at: "2026-07-25T10:05:00Z" };
    const [view] = buildChatMessages([edited], [], [], "me");
    expect(view.reactions).toEqual([{ emoji: "❤️", count: 2, reacted: true }]);
    expect(view.edited).toBe(true);
    expect(view.deleted).toBe(false);

    // A deleted message still renders (redacted), never dropped from the stream.
    const gone = { ...base, body: "message deleted", deleted: true };
    const out = buildChatMessages([gone], [], [], "me");
    expect(out).toHaveLength(1);
    expect(out[0].deleted).toBe(true);
    expect(out[0].reactions).toBeUndefined();
  });
});

describe("reconcilePending", () => {
  it("never cancels a failed send", () => {
    const pending: PendingMessage[] = [{ ...pend("t1", "me", "x"), failed: true }];
    const echo = [msg("r", "me", "x", "2026-07-25T10:00:05Z")];
    expect(reconcilePending(pending, echo)).toHaveLength(1);
  });
});
