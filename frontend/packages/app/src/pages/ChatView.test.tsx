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
