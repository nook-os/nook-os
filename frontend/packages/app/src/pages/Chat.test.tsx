// AC-2 / AC-4 assembly test: the real <ChatPage/> wired to a mocked chat client.
// It proves the surface lists channels, shows history, appends a live websocket
// message without a refetch, and — for the user's own post — renders the
// optimistic bubble exactly once after the server echo arrives (no double
// render). jsdom only, no chat service.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

// Capture the live-socket callbacks so the test can push messages through them.
let liveCallback: ((m: unknown) => void) | null = null;
let updateCallback: ((m: unknown) => void) | null = null;
// Presence/typing frames (MAIN-163) arrive on the same stream.
let presenceCallback: ((p: unknown) => void) | null = null;
let typingCallback: ((t: unknown) => void) | null = null;
const dispose = vi.fn();
// The caller's chat role, mutable so the admin-gate tests can flip it (AC-5).
const identity = vi.hoisted(() => ({ role: "member" as string | null }));
// The channel list the mocked client returns, as a mutable server-of-record so
// unread-badge tests (MAIN-117) can change it between refetches — a background
// message raises a count, marking read zeroes it.
const chatState = vi.hoisted(() => ({
  channels: [] as any[],
  categories: [] as any[],
  dms: [] as any[],
}));

vi.mock("@nookos/api", () => ({
  // ChatView (in @nookos/ui) imports this from the same module; the whole-module
  // mock must provide it or the picker crashes.
  ALLOWED_REACTIONS: ["👍", "👎", "❤️", "😄", "🎉", "😕", "🚀", "👀", "🙌", "🔥", "✅", "❌"],
  api: {
    GET: vi.fn(async (path: string) =>
      // `/api/v1/config` is the deployment's optional-feature surface. No Giphy
      // key here, which is the shipped state and the one AC-3 pins.
      path === "/api/v1/config"
        ? { data: { giphy_key: null } }
        : { data: { user: { id: "me" } } },
    ),
  },
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
  // Channel categories (MAIN-179).
  listCategories: vi.fn(async () => chatState.categories),
  placeChannel: vi.fn(async () => undefined),
  reorderCategories: vi.fn(async () => chatState.categories),
  createCategory: vi.fn(),
  renameCategory: vi.fn(),
  deleteCategory: vi.fn(),
  listDms: vi.fn(async () => chatState.dms),
  sendTyping: vi.fn(),
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
    (
      onMessage: (m: unknown) => void,
      handlers?: {
        onUpdate?: (m: unknown) => void;
        onPresence?: (p: unknown) => void;
        onTyping?: (t: unknown) => void;
      },
    ) => {
      liveCallback = onMessage;
      updateCallback = handlers?.onUpdate ?? null;
      presenceCallback = handlers?.onPresence ?? null;
      typingCallback = handlers?.onTyping ?? null;
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
import { listChannels, markRead, sendTyping } from "@nookos/api";
import { TYPING_PING_MS, TYPING_TTL_MS } from "./chatPresence";

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <ChatPage />
    </QueryClientProvider>,
  );
}

// The width the page is rendered at. jsdom's own default is the desktop one,
// restored before every test so a compact case cannot leave the next on a phone.
const DESKTOP_WIDTH = 1024;
const PHONE_WIDTH = 375;

function setViewport(width: number) {
  Object.defineProperty(window, "innerWidth", { value: width, configurable: true });
}

beforeEach(() => {
  liveCallback = null;
  updateCallback = null;
  presenceCallback = null;
  typingCallback = null;
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
      category_id: null,
      position: 0,
      created_at: "2026-07-25T09:00:00Z",
    },
  ];
  chatState.categories = [];
  chatState.dms = [];
  // jsdom reports focus inconsistently; force it so the open/focus mark-read
  // effect (AC-3) runs deterministically.
  vi.spyOn(document, "hasFocus").mockReturnValue(true);
  localStorage.clear(); // category collapse state must not leak between tests
  setViewport(DESKTOP_WIDTH);
});
afterEach(() => cleanup());

// A category and two channels (one under it, one uncategorized) for the grouping
// tests (MAIN-179).
function withCategories() {
  chatState.categories = [
    { id: "cat1", name: "Team", owner_type: "tenant", position: 0, created_at: "2026-07-25T09:00:00Z" },
  ];
  chatState.channels = [
    { id: "c1", name: "general", slug: "general", owner_type: "tenant", archived: false, unread_count: 0, category_id: null, position: 0, created_at: "2026-07-25T09:00:00Z" },
    { id: "c2", name: "standup", slug: "standup", owner_type: "tenant", archived: false, unread_count: 0, category_id: "cat1", position: 0, created_at: "2026-07-25T09:00:00Z" },
  ];
}

describe("ChatPage channel categories (MAIN-179)", () => {
  it("groups channels under category headers with uncategorized on top (AC-1)", async () => {
    identity.role = "admin";
    withCategories();
    renderPage();
    // Scope to the sidebar — the active channel's name also shows in the main
    // header, so match within the "Channels" aside.
    const aside = await screen.findByLabelText("Channels");
    expect(await within(aside).findByText("(uncategorized)")).toBeTruthy();
    expect(within(aside).getByText("Team")).toBeTruthy();
    expect(within(aside).getByText("general")).toBeTruthy();
    expect(within(aside).getByText("standup")).toBeTruthy();
    // Uncategorized renders before the category.
    const uncat = within(aside).getByText("(uncategorized)");
    const team = within(aside).getByText("Team");
    expect(uncat.compareDocumentPosition(team) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  it("collapsing a category hides its channels and persists (AC-1)", async () => {
    identity.role = "admin";
    withCategories();
    renderPage();
    await screen.findByText("standup");
    fireEvent.click(await screen.findByLabelText("collapse Team"));
    await waitFor(() => expect(screen.queryByText("standup")).toBeNull());
    expect(
      JSON.parse(localStorage.getItem("nook.chat.collapsedCategories") ?? "[]"),
    ).toContain("cat1");
  });

  it("a non-admin sees the grouped list but no drag handle (AC-4)", async () => {
    identity.role = "member";
    withCategories();
    renderPage();
    expect(await screen.findByText("Team")).toBeTruthy();
    // The header's drag-handle class is admin-only.
    const head = screen.getByText("Team").closest(".chat-cat-head");
    expect(head?.classList.contains("draggable")).toBe(false);
  });
});

// MAIN-163: presence dots and the typing line, driven through the real page by
// the frames the stream delivers. Fake timers because the whole contract is a
// clock — a 4s local expiry and a 3s send throttle — and `shouldAdvanceTime`
// keeps react-query's own awaits working while the test jumps that clock.
// MAIN-498: on a phone the channel list is a drawer over the message view. The
// stylesheet's half is asserted in `chatDrawerStyles.test.ts` — jsdom evaluates
// no media query, so what a rendered test can prove is which controls exist and
// what each of them does.
describe("ChatPage channel drawer at compact (MAIN-498)", () => {
  const openDrawer = async () => {
    await userEvent.click(await screen.findByLabelText("show channels"));
    return screen.findByLabelText("Channels");
  };

  beforeEach(() => setViewport(PHONE_WIDTH));

  it("keeps the list out of the page until the header control opens it (AC-1/AC-2)", async () => {
    renderPage();
    // The room is the whole page: the rail is not merely narrow, it is absent.
    await screen.findByText("old message");
    expect(screen.queryByLabelText("Channels")).toBeNull();

    const drawer = await openDrawer();
    expect(within(drawer).getByText("general")).toBeTruthy();
  });

  it("closes on a scrim tap (AC-2)", async () => {
    const { container } = renderPage();
    await openDrawer();
    const scrim = container.querySelector(".chat-drawer-scrim");
    expect(scrim).not.toBeNull();
    await userEvent.click(scrim as Element);
    await waitFor(() => expect(screen.queryByLabelText("Channels")).toBeNull());
  });

  it("closes from the control in its own head", async () => {
    // Not one of AC-2's three routes, but the drawer takes 86vw and the scrim
    // left to tap is a sliver — the same close `.git-drawer-close` gives its
    // drawer, for the same reason.
    renderPage();
    const drawer = await openDrawer();
    await userEvent.click(within(drawer).getByLabelText("close channels"));
    await waitFor(() => expect(screen.queryByLabelText("Channels")).toBeNull());
  });

  it("closes on Escape (AC-2)", async () => {
    renderPage();
    await openDrawer();
    // On `window`, not on the drawer: the key has to land wherever focus
    // happens to be, and after a tap that is the toggle in the header.
    fireEvent.keyDown(window, { key: "Escape" });
    await waitFor(() => expect(screen.queryByLabelText("Channels")).toBeNull());
  });

  it("closes when a channel is picked, and opens that channel (AC-2)", async () => {
    chatState.channels = [
      { id: "c1", name: "general", slug: "general", owner_type: "tenant", archived: false, unread_count: 0, category_id: null, position: 0, created_at: "2026-07-25T09:00:00Z" },
      { id: "c2", name: "random", slug: "random", owner_type: "tenant", archived: false, unread_count: 0, category_id: null, position: 1, created_at: "2026-07-25T09:00:00Z" },
    ];
    renderPage();
    const drawer = await openDrawer();
    await userEvent.click(within(drawer).getByText("random"));

    await waitFor(() => expect(screen.queryByLabelText("Channels")).toBeNull());
    // The pick landed: the header names the channel the drawer just closed on.
    expect(await screen.findByText("random")).toBeTruthy();
  });

  it("closes when a DM is picked (AC-2)", async () => {
    chatState.dms = [
      {
        id: "d1",
        created_at: "2026-07-25T09:00:00Z",
        unread_count: 0,
        participants: [
          { person_id: "p-me", display_name: "Me" },
          { person_id: "p-bob", display_name: "Bob" },
        ],
      },
    ];
    renderPage();
    const drawer = await openDrawer();
    await userEvent.click(within(drawer).getByText("Bob"));
    await waitFor(() => expect(screen.queryByLabelText("Channels")).toBeNull());
  });

  it("renders categories and unread badges inside the drawer (AC-3)", async () => {
    identity.role = "admin";
    withCategories();
    chatState.channels = chatState.channels.map((c) =>
      c.id === "c2" ? { ...c, unread_count: 7 } : c,
    );
    renderPage();
    const drawer = await openDrawer();
    // Nothing is dropped for being small: the same groups, in the same order,
    // with the same count pill the rail shows.
    expect(within(drawer).getByText("(uncategorized)")).toBeTruthy();
    expect(within(drawer).getByText("Team")).toBeTruthy();
    expect(within(drawer).getByText("standup")).toBeTruthy();
    expect(within(drawer).getByLabelText("7 unread")).toBeTruthy();
  });

  it("forgets an open drawer on the way out of compact, so narrowing back does not reopen it", async () => {
    renderPage();
    await openDrawer();

    // Widen past the breakpoint: the rail is back, and the drawer's own chrome
    // is gone with it.
    act(() => {
      setViewport(DESKTOP_WIDTH);
      window.dispatchEvent(new Event("resize"));
    });
    await waitFor(() => expect(screen.queryByLabelText("show channels")).toBeNull());

    // …and narrowing back lands on the message view, not on a drawer nobody
    // asked for a second time.
    act(() => {
      setViewport(PHONE_WIDTH);
      window.dispatchEvent(new Event("resize"));
    });
    await waitFor(() => expect(screen.queryByLabelText("show channels")).not.toBeNull());
    expect(screen.queryByLabelText("Channels")).toBeNull();
  });

  it("above the breakpoint there is a rail and no drawer chrome at all (AC-7)", async () => {
    setViewport(DESKTOP_WIDTH);
    const { container } = renderPage();
    expect(await screen.findByLabelText("Channels")).toBeTruthy();
    expect(screen.queryByLabelText("show channels")).toBeNull();
    expect(screen.queryByLabelText("close channels")).toBeNull();
    expect(container.querySelector(".chat-drawer-scrim")).toBeNull();
  });
});

describe("ChatPage presence and typing (MAIN-163)", () => {
  beforeEach(() => vi.useFakeTimers({ shouldAdvanceTime: true }));
  afterEach(() => vi.useRealTimers());

  function withDm() {
    chatState.dms = [
      {
        id: "d1",
        created_at: "2026-07-25T09:00:00Z",
        unread_count: 0,
        participants: [
          { person_id: "p-me", display_name: "Me" },
          { person_id: "p-bob", display_name: "Bob" },
        ],
      },
    ];
  }

  it("shows a DM's dot on an online frame and clears it on the offline one", async () => {
    withDm();
    renderPage();
    await screen.findByText("Bob");
    await waitFor(() => expect(presenceCallback).not.toBeNull());
    // Nothing has been said about Bob yet: no dot. Absent is not offline.
    expect(screen.queryByLabelText("Bob is online")).toBeNull();

    act(() => presenceCallback!({ person: "p-bob", online: true }));
    expect(await screen.findByLabelText("Bob is online")).toBeTruthy();

    act(() => presenceCallback!({ person: "p-bob", online: false }));
    await waitFor(() => expect(screen.queryByLabelText("Bob is online")).toBeNull());
  });

  it("never dots a DM from someone else's presence", async () => {
    withDm();
    renderPage();
    await screen.findByText("Bob");
    await waitFor(() => expect(presenceCallback).not.toBeNull());
    act(() => presenceCallback!({ person: "p-carol", online: true }));
    await waitFor(() => expect(screen.queryByLabelText("Bob is online")).toBeNull());
  });

  it("never dots a DM from the viewer's own presence", async () => {
    withDm();
    renderPage();
    await screen.findByText("Bob");
    await waitFor(() => expect(presenceCallback).not.toBeNull());
    // The server announces the caller's own edges too — a DM row must not
    // report itself online because we are.
    act(() => presenceCallback!({ person: "p-me", online: true }));
    await waitFor(() => expect(screen.queryByLabelText("Bob is online")).toBeNull());
  });

  it("shows the typing line and expires it locally after the TTL", async () => {
    renderPage();
    await screen.findByText("old message");
    await waitFor(() => expect(typingCallback).not.toBeNull());

    act(() => typingCallback!({ channel_id: "c1", person: "p-ada", display_name: "Ada" }));
    expect(await screen.findByText("Ada is typing…")).toBeTruthy();

    // There is no stop frame — the client is the only thing that ends it.
    act(() => void vi.advanceTimersByTime(TYPING_TTL_MS + 1));
    await waitFor(() => expect(screen.queryByText("Ada is typing…")).toBeNull());
  });

  it("combines two typists into one line", async () => {
    renderPage();
    await screen.findByText("old message");
    await waitFor(() => expect(typingCallback).not.toBeNull());
    act(() => {
      typingCallback!({ channel_id: "c1", person: "p-ada", display_name: "Ada" });
      typingCallback!({ channel_id: "c1", person: "p-bob", display_name: "Bob" });
    });
    expect(await screen.findByText("Ada and Bob are typing…")).toBeTruthy();
  });

  it("ignores typing in another channel, and its own echo", async () => {
    renderPage();
    await screen.findByText("old message");
    await waitFor(() => expect(typingCallback).not.toBeNull());
    act(() => {
      typingCallback!({ channel_id: "c2", person: "p-ada", display_name: "Ada" });
      // The server fans the frame back to the typist too; showing yourself
      // typing is nonsense.
      typingCallback!({ channel_id: "c1", person: "p-me", display_name: "Me" });
    });
    await waitFor(() => expect(screen.queryByText(/is typing…/)).toBeNull());
  });

  it("sends at most one typing ping per throttle window", async () => {
    renderPage();
    const input = (await screen.findByLabelText("Message")) as HTMLTextAreaElement;

    fireEvent.change(input, { target: { value: "h" } });
    fireEvent.change(input, { target: { value: "he" } });
    fireEvent.change(input, { target: { value: "hel" } });
    expect(vi.mocked(sendTyping).mock.calls).toEqual([["c1"]]);

    act(() => void vi.advanceTimersByTime(TYPING_PING_MS));
    fireEvent.change(input, { target: { value: "hell" } });
    expect(vi.mocked(sendTyping).mock.calls).toEqual([["c1"], ["c1"]]);
  });

  it("does not ping for a box the user just emptied", async () => {
    renderPage();
    const input = (await screen.findByLabelText("Message")) as HTMLTextAreaElement;
    fireEvent.change(input, { target: { value: "" } });
    expect(vi.mocked(sendTyping)).not.toHaveBeenCalled();
  });
});

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
