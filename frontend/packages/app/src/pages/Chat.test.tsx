// AC-2 / AC-4 assembly test: the real <ChatPage/> wired to a mocked chat client.
// It proves the surface lists channels, shows history, appends a live websocket
// message without a refetch, and — for the user's own post — renders the
// optimistic bubble exactly once after the server echo arrives (no double
// render). jsdom only, no chat service.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

// Capture the live-socket callbacks so the test can push messages through them.
let liveCallback: ((m: unknown) => void) | null = null;
let updateCallback: ((m: unknown) => void) | null = null;
const dispose = vi.fn();
// The caller's chat role, mutable so the admin-gate tests can flip it (AC-5).
const identity = vi.hoisted(() => ({ role: "member" as string | null }));
// The channel list the mocked client returns, as a mutable server-of-record so
// unread-badge tests (MAIN-117) can change it between refetches — a background
// message raises a count, marking read zeroes it.
const chatState = vi.hoisted(() => ({ channels: [] as any[] }));

vi.mock("@nookos/api", () => ({
  // ChatView (in @nookos/ui) imports this from the same module; the whole-module
  // mock must provide it or the picker crashes.
  ALLOWED_REACTIONS: ["👍", "👎", "❤️", "😄", "🎉", "😕", "🚀", "👀", "🙌", "🔥", "✅", "❌"],
  api: { GET: vi.fn(async () => ({ data: { user: { id: "me" } } })) },
  me: vi.fn(async () => ({
    user_id: "me",
    tenant_id: "t",
    person_id: "p-me",
    cookie_session: false,
    role: identity.role,
  })),
  createChannel: vi.fn(),
  updateChannel: vi.fn(),
  listChannels: vi.fn(async () => chatState.channels),
  listDms: vi.fn(async () => []),
  // Marking a conversation read zeroes its unread on the server-of-record, so the
  // resync after it clears the badge (MAIN-117 AC-3).
  markRead: vi.fn(async (id: string) => {
    const c = chatState.channels.find((x) => x.id === id);
    if (c) c.unread_count = 0;
    return undefined;
  }),
  openDm: vi.fn(),
  listPeople: vi.fn(async () => []),
  channelHistory: vi.fn(async () => ({
    messages: [
      { id: "h1", author_id: "u-bob", channel_id: "c1", body: "old message", created_at: "2026-07-25T09:30:00Z" },
    ],
    next_cursor: null,
  })),
  postMessage: vi.fn(async (_channel: string, body: string) => ({
    id: "real1",
    author_id: "me",
    channel_id: "c1",
    body,
    created_at: "2026-07-25T10:00:05Z",
  })),
  // MAIN-114: opening a thread fetches the parent + its replies. The parent here
  // is the history message "old message"; the thread starts empty.
  messageThread: vi.fn(async () => ({
    parent: {
      id: "h1",
      author_id: "u-bob",
      channel_id: "c1",
      body: "old message",
      created_at: "2026-07-25T09:30:00Z",
    },
    replies: [],
    next_cursor: null,
  })),
  connectChatStream: vi.fn(
    (onMessage: (m: unknown) => void, handlers?: { onUpdate?: (m: unknown) => void }) => {
      liveCallback = onMessage;
      updateCallback = handlers?.onUpdate ?? null;
      return dispose;
    },
  ),
  toggleReaction: vi.fn(async (id: string, emoji: string, on: boolean) => ({
    id,
    author_id: "u-bob",
    channel_id: "c1",
    body: "old message",
    created_at: "2026-07-25T09:30:00Z",
    reactions: [{ emoji, count: 1, reacted: on }],
  })),
  editMessage: vi.fn(),
  deleteMessage: vi.fn(),
}));

import { ChatPage } from "./Chat";
import { listChannels, markRead } from "@nookos/api";

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <ChatPage />
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  liveCallback = null;
  updateCallback = null;
  identity.role = "member";
  dispose.mockClear();
  vi.mocked(markRead).mockClear();
  vi.mocked(listChannels).mockClear();
  // A single caught-up channel by default; badge tests override this.
  chatState.channels = [
    {
      id: "c1",
      name: "general",
      slug: "general",
      owner_type: "tenant",
      archived: false,
      unread_count: 0,
      created_at: "2026-07-25T09:00:00Z",
    },
  ];
  // jsdom reports focus inconsistently; force it so the open/focus mark-read
  // effect (AC-3) runs deterministically.
  vi.spyOn(document, "hasFocus").mockReturnValue(true);
});
afterEach(() => cleanup());

describe("ChatPage", () => {
  it("lists channels and shows history", async () => {
    renderPage();
    expect(await screen.findByText("general")).toBeTruthy();
    expect(await screen.findByText("old message")).toBeTruthy();
  });

  it("appends a live websocket message without a refetch", async () => {
    renderPage();
    await screen.findByText("old message");
    await waitFor(() => expect(liveCallback).not.toBeNull());
    // The real client unwraps the `{type,data}` frame and hands the page the
    // inner ChatMessage; the mock passes that unwrapped shape straight through.
    act(() => {
      liveCallback!({
        id: "live1",
        author_id: "u-carol",
        channel_id: "c1",
        body: "live hello",
        created_at: "2026-07-25T10:01:00Z",
      });
    });
    expect(await screen.findByText("live hello")).toBeTruthy();
  });

  // MAIN-117 AC-5/AC-6: a live message for a conversation the user is NOT viewing
  // bumps that conversation's unread badge at once (via a list resync), without
  // showing the message in the open channel and without polling.
  it("bumps a background conversation's unread badge on a live message", async () => {
    chatState.channels = [
      { id: "c1", name: "general", slug: "general", owner_type: "tenant", archived: false, unread_count: 0, created_at: "2026-07-25T09:00:00Z" },
      { id: "c2", name: "random", slug: "random", owner_type: "tenant", archived: false, unread_count: 0, created_at: "2026-07-25T09:00:00Z" },
    ];
    renderPage();
    await screen.findByText("general");
    await screen.findByText("random");
    await waitFor(() => expect(liveCallback).not.toBeNull());
    // c1 is auto-selected; both conversations start caught up, so no badge shows.
    expect(screen.queryByLabelText(/unread/)).toBeNull();

    // The server now reports an unread message in c2; a live frame for c2 (NOT the
    // open channel) drives the resync that surfaces its badge.
    chatState.channels = chatState.channels.map((c) =>
      c.id === "c2" ? { ...c, unread_count: 3 } : c,
    );
    act(() => {
      liveCallback!({
        id: "bg1",
        author_id: "u-carol",
        channel_id: "c2",
        body: "psst — over here",
        created_at: "2026-07-25T10:02:00Z",
      });
    });

    expect(await screen.findByLabelText("3 unread")).toBeTruthy();
    // The background message never leaks into the open channel's view.
    expect(screen.queryByText("psst — over here")).toBeNull();
  });

  // MAIN-117 AC-3: opening a conversation advances its read cursor and the resync
  // clears its unread badge.
  it("marks the open conversation read and clears its badge", async () => {
    chatState.channels = [
      { id: "c1", name: "general", slug: "general", owner_type: "tenant", archived: false, unread_count: 5, created_at: "2026-07-25T09:00:00Z" },
    ];
    renderPage();
    await screen.findByText("general");
    // Opening c1 (auto-selected) marks it read…
    await waitFor(() => expect(vi.mocked(markRead)).toHaveBeenCalledWith("c1"));
    // …and the badge that was showing "5 unread" clears on the resync.
    await waitFor(() => expect(screen.queryByLabelText(/unread/)).toBeNull());
  });

  it("dedupes an optimistic post against its server echo", async () => {
    renderPage();
    await screen.findByText("old message");
    const input = (await screen.findByLabelText("Message")) as HTMLTextAreaElement;

    await userEvent.type(input, "my post");
    await userEvent.click(screen.getByText("Send"));

    // The optimistic bubble is shown.
    expect(await screen.findByText("my post")).toBeTruthy();

    // The websocket delivers the echo of our own post (same id postMessage
    // returned). It must reconcile, not double-render.
    await waitFor(() => expect(liveCallback).not.toBeNull());
    act(() => {
      liveCallback!({
        id: "real1",
        author_id: "me",
        channel_id: "c1",
        body: "my post",
        created_at: "2026-07-25T10:00:05Z",
      });
    });

    await waitFor(() => expect(screen.getAllByText("my post")).toHaveLength(1));
  });

  it("tears down the socket on unmount", async () => {
    const { unmount } = renderPage();
    await waitFor(() => expect(liveCallback).not.toBeNull());
    unmount();
    expect(dispose).toHaveBeenCalled();
  });

  // AC-5: the management affordance is gated on the caller's chat role.
  it("hides the manage control from a non-admin", async () => {
    identity.role = "member";
    renderPage();
    await screen.findByText("general");
    expect(screen.queryByLabelText("manage channels")).toBeNull();
  });

  it("shows the manage control to an admin", async () => {
    identity.role = "admin";
    renderPage();
    await screen.findByText("general");
    expect(await screen.findByLabelText("manage channels")).toBeTruthy();
  });

  // MAIN-116 AC-5: a message_updated edit is applied in place, no refetch.
  it("applies a message_updated edit in place", async () => {
    renderPage();
    await screen.findByText("old message");
    await waitFor(() => expect(updateCallback).not.toBeNull());
    act(() => {
      updateCallback!({
        id: "h1",
        author_id: "u-bob",
        channel_id: "c1",
        body: "edited message",
        created_at: "2026-07-25T09:30:00Z",
        edited_at: "2026-07-25T10:00:00Z",
      });
    });
    expect(await screen.findByText("edited message")).toBeTruthy();
    expect(screen.getByText("(edited)")).toBeTruthy();
    expect(screen.queryByText("old message")).toBeNull();
  });

  // MAIN-116 AC-4: a soft-delete arriving over the socket redacts in place.
  it("renders a message_updated soft-delete as a placeholder", async () => {
    renderPage();
    await screen.findByText("old message");
    await waitFor(() => expect(updateCallback).not.toBeNull());
    act(() => {
      updateCallback!({
        id: "h1",
        author_id: "u-bob",
        channel_id: "c1",
        body: "message deleted",
        created_at: "2026-07-25T09:30:00Z",
        deleted: true,
      });
    });
    expect(await screen.findByText("message deleted")).toBeTruthy();
    expect(screen.queryByText("old message")).toBeNull();
  });

  // MAIN-116 AC-2: reacting folds the REST response into the stream at once.
  it("shows a reaction pill after toggling a reaction", async () => {
    renderPage();
    await screen.findByText("old message");
    await userEvent.click(await screen.findByLabelText("Add reaction"));
    await userEvent.click(await screen.findByLabelText("React with 👍"));
    expect(await screen.findByLabelText(/👍 1/)).toBeTruthy();
  });

  // MAIN-114 AC-5: opening a message's thread mounts the thread panel beside the
  // channel view — the parent is pinned and its (empty) reply list renders.
  it("opens the thread panel from a message's reply affordance", async () => {
    renderPage();
    await screen.findByText("old message");

    await userEvent.click(await screen.findByLabelText("Reply in thread"));

    // The panel appeared: its close control, pinned parent, and empty reply list.
    expect(await screen.findByLabelText("Close thread")).toBeTruthy();
    expect(await screen.findByText("No replies yet.")).toBeTruthy();
    // The parent body now shows in both the channel stream and the pinned parent.
    await waitFor(() => expect(screen.getAllByText("old message")).toHaveLength(2));
  });
});
