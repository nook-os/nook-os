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
    // Edit now lives behind the bar's three-dots menu (MAIN-300 AC-1).
    await userEvent.click(screen.getByLabelText("More actions"));
    await userEvent.click(screen.getByLabelText("Edit message"));
    const box = screen.getByLabelText("Edit message") as HTMLTextAreaElement;
    await userEvent.clear(box);
    await userEvent.type(box, "fixed body{Enter}");
    expect(onEditMessage).toHaveBeenCalledWith("r1", "fixed body");
  });

  it("offers no edit/delete on another user's message", async () => {
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
    // Open the menu before asserting: "absent" has to mean absent from an OPEN
    // menu, or this would pass merely because nothing had been clicked yet.
    await userEvent.click(screen.getByLabelText("More actions"));
    expect(screen.queryByLabelText("Edit message")).toBeNull();
    expect(screen.queryByLabelText("Delete message")).toBeNull();
    // What is left is the action that needs no permission and no caller.
    expect(screen.getByLabelText("Copy text")).toBeTruthy();
    // …and anyone may react.
    expect(screen.getByLabelText("Add reaction")).toBeTruthy();
  });

  it("lets a tenant admin delete (but not edit) another user's message (AC-4)", async () => {
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
    await userEvent.click(screen.getByLabelText("More actions"));
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
    const { container } = render(
      <ChatView
        messages={[deleted]}
        onSend={vi.fn()}
        currentUserId="u-me"
        onToggleReaction={vi.fn()}
        onEditMessage={vi.fn()}
        onDeleteMessage={vi.fn()}
        onOpenThread={vi.fn()}
      />,
    );
    expect(screen.getByText("message deleted")).toBeTruthy();
    // AC-4: no bar at all, so there is nothing to open and nothing to assert
    // about individually.
    expect(container.querySelector(".chat-msg-bar")).toBeNull();
    expect(screen.queryByLabelText("More actions")).toBeNull();
    expect(screen.queryByLabelText("Add reaction")).toBeNull();
    expect(screen.queryByLabelText("Reply in thread")).toBeNull();
    // The suppressed pill must not render either.
    expect(screen.queryByLabelText(/remove your reaction/)).toBeNull();
  });
});

// MAIN-300: the actions moved OUT of the message row into a bar that floats over
// its upper-right corner, with edit/delete/copy behind a three-dots menu.
//
// Caveat these tests carry openly: jsdom applies no stylesheet, so none of them
// can prove the bar is INVISIBLE at rest or that hover reveals it — that is CSS,
// and it is verified in the running app (AC-5). What they do prove is the part
// jsdom can see and that CSS depends on: that the actions are no longer rendered
// in the row's flow, that the bar is a child of the message with the class the
// hover/focus rules key off, and that every action still reaches its callback.
describe("ChatView floating action bar (MAIN-300)", () => {
  const mine: ChatViewMessage = {
    id: "b1",
    authorId: "u-me",
    authorName: "me",
    body: "hover me",
    createdAt: "2026-07-25T10:00:00Z",
  };

  const wired = (over: Partial<React.ComponentProps<typeof ChatView>> = {}) => (
    <ChatView
      messages={[mine]}
      onSend={vi.fn()}
      currentUserId="u-me"
      onToggleReaction={vi.fn()}
      onEditMessage={vi.fn()}
      onDeleteMessage={vi.fn()}
      onOpenThread={vi.fn()}
      {...over}
    />
  );

  it("puts the actions in a bar on the message, and nothing in the row (AC-1/AC-2)", () => {
    const { container } = render(wired());
    const row = container.querySelector(".chat-msg")!;
    const bar = row.querySelector(":scope > .chat-msg-bar");
    expect(bar).toBeTruthy();
    // The old in-flow row is gone — that removal is the density fix (AC-2).
    expect(container.querySelector(".chat-msg-actions")).toBeNull();
    expect(container.querySelector(".chat-act")).toBeNull();
    // Every action button the row renders lives inside the bar, so at rest the
    // row's own content is the header, the body, and nothing else.
    for (const el of row.querySelectorAll("button")) {
      expect(bar!.contains(el)).toBe(true);
    }
    // Quick actions sit in the bar; edit/delete/copy are behind the three dots.
    expect(bar!.querySelector('[aria-label="Add reaction"]')).toBeTruthy();
    expect(bar!.querySelector('[aria-label="Reply in thread"]')).toBeTruthy();
    expect(bar!.querySelector('[aria-label="More actions"]')).toBeTruthy();
    expect(screen.queryByLabelText("Edit message")).toBeNull();
    expect(screen.queryByLabelText("Delete message")).toBeNull();
  });

  it("renders no reply row when a message has no replies, and the count when it does", () => {
    const { container, rerender } = render(wired());
    expect(container.querySelector(".chat-msg-thread")).toBeNull();
    rerender(wired({ messages: [{ ...mine, replyCount: 2 }] }));
    expect(container.querySelector(".chat-msg-thread")).toBeTruthy();
    expect(screen.getByText(/2\s+replies/)).toBeTruthy();
  });

  it("opens the thread from the bar's reply action (AC-5)", async () => {
    const onOpenThread = vi.fn();
    render(wired({ onOpenThread }));
    await userEvent.click(screen.getByLabelText("Reply in thread"));
    expect(onOpenThread).toHaveBeenCalledWith(expect.objectContaining({ id: "b1" }));
  });

  it("holds edit, copy and delete behind the three-dots menu (AC-1)", async () => {
    const onDeleteMessage = vi.fn();
    render(wired({ onDeleteMessage }));
    await userEvent.click(screen.getByLabelText("More actions"));
    const menu = screen.getByRole("menu");
    expect(menu.textContent).toContain("Edit");
    expect(menu.textContent).toContain("Copy text");
    expect(menu.textContent).toContain("Delete");
    await userEvent.click(screen.getByLabelText("Delete message"));
    expect(onDeleteMessage).toHaveBeenCalledWith("b1");
    // Choosing an item closes the menu.
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("copies the body to the clipboard (AC-1)", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    const original = navigator.clipboard;
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText },
      configurable: true,
    });
    try {
      render(wired());
      await userEvent.click(screen.getByLabelText("More actions"));
      await userEvent.click(screen.getByLabelText("Copy text"));
      expect(writeText).toHaveBeenCalledWith("hover me");
    } finally {
      Object.defineProperty(navigator, "clipboard", {
        value: original,
        configurable: true,
      });
    }
  });

  it("is a real menu: Esc closes it and gives focus back to the trigger (AC-3)", async () => {
    render(wired());
    const trigger = screen.getByLabelText("More actions");
    await userEvent.click(trigger);
    // Opening moves focus INTO the menu, so a keyboard user has somewhere to be.
    expect(screen.getByRole("menu").contains(document.activeElement)).toBe(true);
    await userEvent.keyboard("{Escape}");
    expect(screen.queryByRole("menu")).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });

  it("closes the reaction picker on Esc too, returning focus (AC-3)", async () => {
    render(wired());
    const trigger = screen.getByLabelText("Add reaction");
    await userEvent.click(trigger);
    expect(screen.getByLabelText("React with 🚀")).toBeTruthy();
    await userEvent.keyboard("{Escape}");
    expect(screen.queryByLabelText("React with 🚀")).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });

  it("read-only: with no callbacks wired, only Copy is offered (AC-4)", async () => {
    render(<ChatView messages={[mine]} onSend={vi.fn()} currentUserId="u-me" />);
    expect(screen.queryByLabelText("Add reaction")).toBeNull();
    expect(screen.queryByLabelText("Reply in thread")).toBeNull();
    await userEvent.click(screen.getByLabelText("More actions"));
    expect(screen.getByLabelText("Copy text")).toBeTruthy();
    expect(screen.queryByLabelText("Edit message")).toBeNull();
    expect(screen.queryByLabelText("Delete message")).toBeNull();
  });

  it("offers nothing on a message that has not settled", () => {
    const { container, rerender } = render(wired({ messages: [{ ...mine, pending: true }] }));
    expect(container.querySelector(".chat-msg-bar")).toBeNull();
    rerender(wired({ messages: [{ ...mine, failed: true }] }));
    expect(container.querySelector(".chat-msg-bar")).toBeNull();
  });
});

// MAIN-237: the shared surface both team chat and the loop render through.
// These cover the props the loop added, since team chat's own behaviour is
// covered above and must not change.
describe("ChatView shared-surface props (MAIN-237)", () => {
  it("shows the activity indicator only when a label is given", () => {
    const { rerender, container } = render(
      <ChatView messages={[]} onSend={() => {}} />,
    );
    expect(container.querySelector(".chat-typing")).toBeNull();

    rerender(
      <ChatView messages={[]} onSend={() => {}} typing="the operator agent is working…" />,
    );
    const el = container.querySelector(".chat-typing");
    expect(el).toBeTruthy();
    expect(el!.textContent).toContain("the operator agent is working…");
    // Announced, not just drawn — this is the cue that the agent is alive.
    expect(el!.getAttribute("role")).toBe("status");

    rerender(<ChatView messages={[]} onSend={() => {}} typing={null} />);
    expect(container.querySelector(".chat-typing")).toBeNull();
  });

  it("renders a caller's controls between the log and the composer", () => {
    const { container } = render(
      <ChatView
        messages={[]}
        onSend={() => {}}
        beforeComposer={<button>choice A</button>}
      />,
    );
    expect(screen.getByText("choice A")).toBeTruthy();
    // Order matters: the ask belongs with the reply, below the conversation.
    const kids = [...container.querySelector(".chat-view")!.children].map(
      (c) => c.className,
    );
    expect(kids[0]).toContain("chat-log");
    expect(kids[kids.length - 1]).toContain("chat-composer");
  });

  it("hides the composer entirely when there is nothing to say to", () => {
    const { container } = render(
      <ChatView messages={[]} onSend={() => {}} hideComposer />,
    );
    expect(container.querySelector(".chat-composer")).toBeNull();
  });

  it("still sends through the callback, unchanged", async () => {
    const onSend = vi.fn();
    render(<ChatView messages={[]} onSend={onSend} />);
    await userEvent.type(screen.getByLabelText("Message"), "hello");
    await userEvent.click(screen.getByText("Send"));
    expect(onSend).toHaveBeenCalledWith("hello");
  });
});
