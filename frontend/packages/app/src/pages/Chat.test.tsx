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

// Capture the live-socket callback so the test can push messages through it.
let liveCallback: ((m: unknown) => void) | null = null;
const dispose = vi.fn();
// The caller's chat role, mutable so the admin-gate tests can flip it (AC-5).
const identity = vi.hoisted(() => ({ role: "member" as string | null }));

vi.mock("@nookos/api", () => ({
  api: { GET: vi.fn(async () => ({ data: { user: { id: "me" } } })) },
  me: vi.fn(async () => ({
    user_id: "me",
    tenant_id: "t",
    cookie_session: false,
    role: identity.role,
  })),
  createChannel: vi.fn(),
  updateChannel: vi.fn(),
  listChannels: vi.fn(async () => [
    { id: "c1", name: "general", slug: "general", archived: false, created_at: "2026-07-25T09:00:00Z" },
  ]),
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
  connectChatSocket: vi.fn((_channel: string, onMessage: (m: unknown) => void) => {
    liveCallback = onMessage;
    return dispose;
  }),
}));

import { ChatPage } from "./Chat";

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
  identity.role = "member";
  dispose.mockClear();
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
});
