// MAIN-529: the `/` palette, the parse, the ephemeral note and the action row —
// driven through the real <ChatView/> with the fake data source the rest of its
// tests use. Naming commands here is deliberate and allowed: a test of the
// palette must name what it is picking, and `serverOwnedCommands.test.ts` is
// what proves nothing OUTSIDE a test does (AC-2).
import React from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import {
  ChatView,
  type ChatViewCommand,
  type ChatViewMessage,
  type ChatViewProps,
} from "@nookos/ui";

afterEach(() => cleanup());

/** Exactly what `GET …/commands` answers today — fixture data, taken verbatim. */
const COMMANDS: ChatViewCommand[] = [
  { name: "help", args_hint: null, description: "List the commands you can use here." },
  { name: "me", args_hint: "<text>", description: "Post what you are doing as an action." },
  { name: "shrug", args_hint: "[text]", description: "Post your text with a shrug on the end." },
];

const MESSAGES: ChatViewMessage[] = [
  { id: "m1", authorId: "u-alice", authorName: "alice", body: "first", createdAt: "2026-08-11T10:00:00Z" },
];

function view(over: Partial<ChatViewProps> = {}) {
  return <ChatView messages={MESSAGES} onSend={vi.fn()} {...over} />;
}

function box(): HTMLTextAreaElement {
  return screen.getByLabelText("Message") as HTMLTextAreaElement;
}

const palette = () => screen.queryByRole("listbox", { name: "Commands" });

describe("the palette opens on a leading slash (AC-3)", () => {
  it("lists what the server gave it — name, argument hint and description", async () => {
    render(view({ commands: COMMANDS, onCommand: vi.fn() }));
    await userEvent.type(box(), "/");

    const list = palette();
    expect(list).toBeTruthy();
    const options = within(list!).getAllByRole("option");
    expect(options.map((o) => o.textContent)).toEqual([
      "/helpList the commands you can use here.",
      "/me<text>Post what you are doing as an action.",
      "/shrug[text]Post your text with a shrug on the end.",
    ]);
  });

  it("filters as more is typed", async () => {
    render(view({ commands: COMMANDS, onCommand: vi.fn() }));
    await userEvent.type(box(), "/he");
    expect(within(palette()!).getAllByRole("option")).toHaveLength(1);
    expect(within(palette()!).getByRole("option").textContent).toContain("/help");
  });

  it("opens nothing for a slash anywhere but the front", async () => {
    render(view({ commands: COMMANDS, onCommand: vi.fn() }));
    await userEvent.type(box(), "look at ./run.sh");
    expect(palette()).toBeNull();
  });

  it("opens nothing when the typed name matches nothing", async () => {
    render(view({ commands: COMMANDS, onCommand: vi.fn() }));
    await userEvent.type(box(), "/nook-spec");
    expect(palette()).toBeNull();
  });
});

describe("keyboard and pointer in the palette (AC-4)", () => {
  it("arrow keys move the selection", async () => {
    render(view({ commands: COMMANDS, onCommand: vi.fn() }));
    await userEvent.type(box(), "/");
    const selected = () =>
      within(palette()!)
        .getAllByRole("option")
        .findIndex((o) => o.getAttribute("aria-selected") === "true");

    expect(selected()).toBe(0);
    await userEvent.keyboard("{ArrowDown}");
    expect(selected()).toBe(1);
    await userEvent.keyboard("{ArrowUp}{ArrowUp}");
    // Wraps rather than sticking at an end — a three-row menu is a ring.
    expect(selected()).toBe(2);
  });

  it("Enter completes the highlighted command and never sends", async () => {
    const onSend = vi.fn();
    const onCommand = vi.fn();
    render(view({ commands: COMMANDS, onCommand, onSend }));
    await userEvent.type(box(), "/");
    await userEvent.keyboard("{ArrowDown}{Enter}");

    expect(box().value).toBe("/me ");
    expect(onSend).not.toHaveBeenCalled();
    expect(onCommand).not.toHaveBeenCalled();
    // Completed, so the palette is done: what follows the space is arguments.
    expect(palette()).toBeNull();
  });

  it("Tab completes too", async () => {
    render(view({ commands: COMMANDS, onCommand: vi.fn() }));
    await userEvent.type(box(), "/sh");
    await userEvent.keyboard("{Tab}");
    expect(box().value).toBe("/shrug ");
  });

  it("clicking a row completes it", async () => {
    render(view({ commands: COMMANDS, onCommand: vi.fn() }));
    await userEvent.type(box(), "/");
    await userEvent.click(within(palette()!).getByText("/shrug"));
    expect(box().value).toBe("/shrug ");
  });

  it("Escape closes it and leaves the typed text alone", async () => {
    const onSend = vi.fn();
    render(view({ commands: COMMANDS, onCommand: vi.fn(), onSend }));
    await userEvent.type(box(), "/he");
    await userEvent.keyboard("{Escape}");

    expect(palette()).toBeNull();
    expect(box().value).toBe("/he");
    expect(onSend).not.toHaveBeenCalled();
  });

  it("dismissal belongs to the query it was typed against, not to the session", async () => {
    // A dismissal that outlived its query left the palette dead for the rest of
    // the composing session, with no way back short of sending something.
    render(view({ commands: COMMANDS, onCommand: vi.fn() }));
    await userEvent.type(box(), "/he");
    await userEvent.keyboard("{Escape}");
    expect(palette()).toBeNull();

    await userEvent.type(box(), "l");
    expect(palette()).toBeTruthy();
  });

  it("a click outside dismisses it, and the next keystroke asks again", async () => {
    render(view({ commands: COMMANDS, onCommand: vi.fn() }));
    await userEvent.type(box(), "/");
    expect(palette()).toBeTruthy();

    // Glancing at the log and clicking it is an ordinary thing to do
    // mid-compose: `useAnchoredMenu` closes on any outside mousedown.
    fireEvent.mouseDown(screen.getByRole("log"));
    expect(palette()).toBeNull();

    await userEvent.type(box(), "h");
    expect(palette()).toBeTruthy();
  });

  it("describes itself as the combobox it behaves as while open", async () => {
    render(view({ commands: COMMANDS, onCommand: vi.fn() }));
    // Closed, the box carries none of this — the tree is what it always was.
    expect(box().getAttribute("role")).toBeNull();

    await userEvent.type(box(), "/");
    expect(box().getAttribute("role")).toBe("combobox");
    expect(box().getAttribute("aria-expanded")).toBe("true");
    expect(box().getAttribute("aria-activedescendant")).toBe("chat-cmd-help");
    await userEvent.keyboard("{ArrowDown}");
    expect(box().getAttribute("aria-activedescendant")).toBe("chat-cmd-me");
  });
});

describe("what submitting does (AC-5/AC-6)", () => {
  it("calls onCommand with the name and the rest of the line, and clears the box", async () => {
    const onCommand = vi.fn(async () => ({}));
    const onSend = vi.fn();
    render(view({ commands: COMMANDS, onCommand, onSend }));
    await userEvent.type(box(), "/shrug ok fine{Enter}");

    expect(onCommand).toHaveBeenCalledWith("shrug", "ok fine");
    expect(onSend).not.toHaveBeenCalled();
    expect(box().value).toBe("");
  });

  it("sends leading-slash text matching NOTHING verbatim — the regression risk", async () => {
    const onCommand = vi.fn();
    const onSend = vi.fn();
    render(view({ commands: COMMANDS, onCommand, onSend }));
    await userEvent.type(box(), "/nook-spec MAIN-1 do a thing{Enter}");

    expect(onSend).toHaveBeenCalledWith("/nook-spec MAIN-1 do a thing");
    expect(onCommand).not.toHaveBeenCalled();
  });

  it("sends everything as before on a surface that passes no commands", async () => {
    const onSend = vi.fn();
    render(view({ onSend }));
    await userEvent.type(box(), "/help{Enter}");
    expect(onSend).toHaveBeenCalledWith("/help");
    expect(palette()).toBeNull();
  });
});

describe("what a command answered with (AC-7)", () => {
  it("renders an ephemeral inline, and posts it nowhere", async () => {
    const onSend = vi.fn();
    const onCommand = vi.fn(async () => ({ ephemeral: "Commands:\n/help — this" }));
    render(view({ commands: COMMANDS, onCommand, onSend }));
    // Two presses: the first completes the highlighted row (AC-4), the second
    // sends what is now in the box.
    await userEvent.type(box(), "/help{Enter}{Enter}");

    await waitFor(() => expect(screen.getByText(/Commands:/)).toBeTruthy());
    expect(onSend).not.toHaveBeenCalled();
  });

  it("renders a refusal the SAME way — not a toast, not raw JSON", async () => {
    const onCommand = vi.fn(async () => {
      throw new Error("Unknown command /nonsense — try /help");
    });
    render(view({ commands: [{ name: "nonsense", description: "a listed name" }], onCommand }));
    await userEvent.type(box(), "/nonsense{Enter}{Enter}");

    await waitFor(() =>
      expect(screen.getByText("Unknown command /nonsense — try /help")).toBeTruthy(),
    );
  });

  it("drops its notes when the conversation changes", async () => {
    const onCommand = vi.fn(async () => ({ ephemeral: "for your eyes only" }));
    const { rerender } = render(
      view({ commands: COMMANDS, onCommand, conversationId: "c-1" }),
    );
    await userEvent.type(box(), "/help{Enter}{Enter}");
    await waitFor(() => expect(screen.getByText("for your eyes only")).toBeTruthy());

    rerender(view({ commands: COMMANDS, onCommand, conversationId: "c-2" }));
    await waitFor(() => expect(screen.queryByText("for your eyes only")).toBeNull());
  });
});

describe("an action message (AC-8)", () => {
  const ACTION: ChatViewMessage = {
    id: "a1",
    authorId: "u-me",
    authorName: "Ryan",
    body: "deploys the thing",
    createdAt: "2026-08-11T10:01:00Z",
    action: true,
    reactions: [{ emoji: "👍", count: 1, reacted: false }],
  };
  const ORDINARY: ChatViewMessage = {
    id: "o1",
    authorId: "u-me",
    authorName: "Ryan",
    body: "says the thing",
    createdAt: "2026-08-11T10:02:00Z",
    reactions: [{ emoji: "👍", count: 1, reacted: false }],
  };

  function withActions(messages: ChatViewMessage[], over: Partial<ChatViewProps> = {}) {
    return view({
      messages,
      currentUserId: "u-me",
      onToggleReaction: vi.fn(),
      onEditMessage: vi.fn(),
      onDeleteMessage: vi.fn(),
      ...over,
    });
  }

  it("renders italic and author-prefixed, on one line", () => {
    const { container } = render(withActions([ACTION]));
    const em = container.querySelector("em");
    expect(em?.textContent).toBe("Ryan deploys the thing");
    expect(container.querySelector(".chat-msg")?.getAttribute("data-kind")).toBe("action");
    // The name is IN the line, so there is no header repeating it above.
    expect(screen.queryByText("Ryan", { selector: ".chat-author" })).toBeNull();
  });

  it("carries no reaction row and offers no Edit", async () => {
    render(withActions([ACTION]));
    expect(screen.queryByLabelText(/👍 1/)).toBeNull();
    expect(screen.queryByLabelText("Add reaction")).toBeNull();

    await userEvent.click(screen.getByLabelText("More actions"));
    expect(screen.queryByLabelText("Edit message")).toBeNull();
    expect(screen.getByLabelText("Delete message")).toBeTruthy();
  });

  it("deletes exactly as an ordinary message does, for the author and for an admin", async () => {
    const onDeleteMessage = vi.fn();
    render(withActions([ACTION], { onDeleteMessage }));
    await userEvent.click(screen.getByLabelText("More actions"));
    await userEvent.click(screen.getByLabelText("Delete message"));
    expect(onDeleteMessage).toHaveBeenCalledWith("a1");
    cleanup();

    // Somebody else's action, seen by a tenant admin.
    const adminDelete = vi.fn();
    render(
      withActions([{ ...ACTION, authorId: "u-alice", authorName: "alice" }], {
        onDeleteMessage: adminDelete,
        canDeleteAny: true,
      }),
    );
    await userEvent.click(screen.getByLabelText("More actions"));
    await userEvent.click(screen.getByLabelText("Delete message"));
    expect(adminDelete).toHaveBeenCalledWith("a1");
  });

  it("leaves an ordinary message exactly as it was", async () => {
    render(withActions([ORDINARY]));
    expect(screen.getByText("says the thing")).toBeTruthy();
    expect(screen.getByLabelText("👍 1")).toBeTruthy();
    await userEvent.click(screen.getByLabelText("More actions"));
    expect(screen.getByLabelText("Edit message")).toBeTruthy();
  });
});

describe("regression: the props are optional (AC-1)", () => {
  it("renders the same tree with them, with them absent, and with a closed palette", () => {
    const bare = render(view()).container.innerHTML;
    cleanup();
    const wired = render(
      view({ commands: COMMANDS, onCommand: vi.fn(), conversationId: "c-1" }),
    ).container.innerHTML;
    // A command list costs nothing until somebody types a slash.
    expect(wired).toBe(bare);
  });

  it("still sends, still clears, still reports typing", async () => {
    const onSend = vi.fn();
    const onTypingActivity = vi.fn();
    render(view({ onSend, onTypingActivity }));
    await userEvent.type(box(), "howdy{Enter}");
    expect(onSend).toHaveBeenCalledWith("howdy");
    expect(box().value).toBe("");
    expect(onTypingActivity).toHaveBeenCalled();
  });
});
