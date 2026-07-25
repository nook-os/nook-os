// AC-4: an optimistic post reconciles with its server echo — no double render.
// This tests the merge logic directly (pure, no DOM/WS), which is where the
// dedupe actually lives.
import { describe, expect, it } from "vitest";
import type { ChatMessage } from "@nookos/api";
import { buildChatMessages, reconcilePending, type PendingMessage } from "./chatMessages";

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
});

describe("reconcilePending", () => {
  it("never cancels a failed send", () => {
    const pending: PendingMessage[] = [{ ...pend("t1", "me", "x"), failed: true }];
    const echo = [msg("r", "me", "x", "2026-07-25T10:00:05Z")];
    expect(reconcilePending(pending, echo)).toHaveLength(1);
  });
});
