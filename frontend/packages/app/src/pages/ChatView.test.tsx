// AC-1 / AC-6: the reusable ChatView is backend-agnostic. This drives it with a
// FAKE data source — plain arrays and vi.fn callbacks, zero chat-service
// dependency — which is exactly what a second consumer (the planned tmux "sugar"
// overlay) would do. If this test compiles and passes, the component is reusable
// for that overlay with no changes: the proof AC-6 asks for.
import React from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ChatView, type ChatViewMessage } from "@nookos/ui";

afterEach(() => cleanup());

const fakeMessages: ChatViewMessage[] = [
  { id: "m1", authorId: "u-alice", authorName: "alice", body: "first", createdAt: "2026-07-25T10:00:00Z" },
  { id: "m2", authorId: "u-alice", authorName: "alice", body: "second", createdAt: "2026-07-25T10:00:10Z" },
  { id: "m3", authorId: "u-bob", authorName: "bob", body: "hello", createdAt: "2026-07-25T10:05:00Z" },
];

describe("ChatView (fake data source)", () => {
  it("renders the message list", () => {
    render(<ChatView messages={fakeMessages} onSend={vi.fn()} />);
    expect(screen.getByText("first")).toBeTruthy();
    expect(screen.getByText("second")).toBeTruthy();
    expect(screen.getByText("hello")).toBeTruthy();
    // Grouped by author: two alice messages, one header for her run.
    expect(screen.getAllByText("alice")).toHaveLength(1);
    expect(screen.getByText("bob")).toBeTruthy();
  });

  it("calls onSend with the typed text and clears the composer", async () => {
    const onSend = vi.fn();
    render(<ChatView messages={fakeMessages} onSend={onSend} />);
    const input = screen.getByLabelText("Message") as HTMLTextAreaElement;
    await userEvent.type(input, "howdy");
    await userEvent.click(screen.getByText("Send"));
    expect(onSend).toHaveBeenCalledWith("howdy");
    expect(input.value).toBe("");
  });

  it("calls onLoadOlder when scrolled to the top and more remains", () => {
    const onLoadOlder = vi.fn();
    render(
      <ChatView
        messages={fakeMessages}
        onSend={vi.fn()}
        onLoadOlder={onLoadOlder}
        hasMore
      />,
    );
    const log = screen.getByRole("log");
    log.scrollTop = 0;
    fireEvent.scroll(log);
    expect(onLoadOlder).toHaveBeenCalledTimes(1);
  });

  it("does not call onLoadOlder when there are no older pages", () => {
    const onLoadOlder = vi.fn();
    render(
      <ChatView messages={fakeMessages} onSend={vi.fn()} onLoadOlder={onLoadOlder} hasMore={false} />,
    );
    const log = screen.getByRole("log");
    log.scrollTop = 0;
    fireEvent.scroll(log);
    expect(onLoadOlder).not.toHaveBeenCalled();
  });
});

// MAIN-116 AC-2/3/4/6: reactions, inline edit, and the deleted placeholder —
// still driven by the fake data source, proving the surface stays reusable.
describe("ChatView reactions/edit/delete", () => {
  const reacted: ChatViewMessage = {
    id: "r1",
    authorId: "u-me",
    authorName: "me",
    body: "reactable",
    createdAt: "2026-07-25T10:00:00Z",
    reactions: [
      { emoji: "👍", count: 2, reacted: true },
      { emoji: "🎉", count: 1, reacted: false },
    ],
  };

  it("renders reaction pills and toggles OFF a highlighted one, ON a plain one", async () => {
    const onToggleReaction = vi.fn();
    render(
      <ChatView
        messages={[reacted]}
        onSend={vi.fn()}
        currentUserId="u-me"
        onToggleReaction={onToggleReaction}
      />,
    );
    // The highlighted pill (reacted) toggles off.
    await userEvent.click(screen.getByLabelText("👍 2, remove your reaction"));
    expect(onToggleReaction).toHaveBeenLastCalledWith("r1", "👍", false);
    // The plain pill toggles on.
    await userEvent.click(screen.getByLabelText("🎉 1"));
    expect(onToggleReaction).toHaveBeenLastCalledWith("r1", "🎉", true);
  });

  it("adds a reaction from the picker with on=true", async () => {
    const onToggleReaction = vi.fn();
    render(
      <ChatView
        messages={[reacted]}
        onSend={vi.fn()}
        currentUserId="u-me"
        onToggleReaction={onToggleReaction}
      />,
    );
    await userEvent.click(screen.getByLabelText("Add reaction"));
    await userEvent.click(screen.getByLabelText("React with 🚀"));
    expect(onToggleReaction).toHaveBeenCalledWith("r1", "🚀", true);
  });

  it("shows an (edited) marker and edits inline on Enter", async () => {
    const onEditMessage = vi.fn();
    const edited: ChatViewMessage = { ...reacted, reactions: undefined, edited: true };
    render(
      <ChatView
        messages={[edited]}
        onSend={vi.fn()}
        currentUserId="u-me"
        onEditMessage={onEditMessage}
      />,
    );
    expect(screen.getByText("(edited)")).toBeTruthy();
    await userEvent.click(screen.getByLabelText("Edit message"));
    const box = screen.getByLabelText("Edit message") as HTMLTextAreaElement;
    await userEvent.clear(box);
    await userEvent.type(box, "fixed body{Enter}");
    expect(onEditMessage).toHaveBeenCalledWith("r1", "fixed body");
  });

  it("offers no edit/delete on another user's message", () => {
    render(
      <ChatView
        messages={[{ ...reacted, authorId: "u-bob" }]}
        onSend={vi.fn()}
        currentUserId="u-me"
        onEditMessage={vi.fn()}
        onDeleteMessage={vi.fn()}
        onToggleReaction={vi.fn()}
      />,
    );
    expect(screen.queryByLabelText("Edit message")).toBeNull();
    expect(screen.queryByLabelText("Delete message")).toBeNull();
    // …but anyone may react.
    expect(screen.getByLabelText("Add reaction")).toBeTruthy();
  });

  it("lets a tenant admin delete (but not edit) another user's message (AC-4)", () => {
    render(
      <ChatView
        messages={[{ ...reacted, authorId: "u-bob" }]}
        onSend={vi.fn()}
        currentUserId="u-me"
        onEditMessage={vi.fn()}
        onDeleteMessage={vi.fn()}
        onToggleReaction={vi.fn()}
        canDeleteAny
      />,
    );
    // Admin gets Delete on someone else's message — the AC-4 admin path.
    expect(screen.getByLabelText("Delete message")).toBeTruthy();
    // Edit stays author-only even for an admin.
    expect(screen.queryByLabelText("Edit message")).toBeNull();
  });

  it("renders a deleted message as a placeholder with no actions or reactions", () => {
    const deleted: ChatViewMessage = {
      ...reacted,
      body: "message deleted",
      deleted: true,
      reactions: [{ emoji: "👍", count: 2, reacted: true }],
    };
    render(
      <ChatView
        messages={[deleted]}
        onSend={vi.fn()}
        currentUserId="u-me"
        onToggleReaction={vi.fn()}
        onEditMessage={vi.fn()}
        onDeleteMessage={vi.fn()}
      />,
    );
    expect(screen.getByText("message deleted")).toBeTruthy();
    expect(screen.queryByLabelText("Add reaction")).toBeNull();
    expect(screen.queryByLabelText("Edit message")).toBeNull();
    expect(screen.queryByLabelText("Delete message")).toBeNull();
    // The suppressed pill must not render either.
    expect(screen.queryByLabelText(/remove your reaction/)).toBeNull();
  });
});
