// MAIN-171: the composer's emoji picker, the GIPHY picker, and how a posted GIF
// renders. Driven through the real <ChatView/> with the fake data source the
// rest of its tests use — only `searchGifs` is stubbed, because the one thing
// this feature must not do in a test is call giphy.com.
//
// The load-bearing case is the LAST describe: with no key the GIF button is
// absent entirely and the emoji picker still works (AC-3). A half-rendered
// feature is the failure this ticket is most likely to ship.
import React from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

const GIF_FULL = "https://media3.giphy.com/media/abc123/giphy.gif";
const GIF_PREVIEW = "https://media3.giphy.com/media/abc123/200w.gif";

vi.mock("@nookos/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@nookos/api")>();
  return {
    ...actual,
    // Everything else — `giphyGifUrl` included — stays real: recognising a GIF
    // message is the behaviour under test, not a fixture.
    searchGifs: vi.fn(async () => [
      { id: "g1", title: "party parrot", previewUrl: GIF_PREVIEW, fullUrl: GIF_FULL },
    ]),
  };
});

import { ChatView, type ChatViewMessage } from "@nookos/ui";
import { searchGifs } from "@nookos/api";

afterEach(() => cleanup());

const KEY = "test-giphy-key";

function composer(over: Partial<React.ComponentProps<typeof ChatView>> = {}) {
  return <ChatView messages={[]} onSend={vi.fn()} {...over} />;
}

describe("emoji picker (AC-1)", () => {
  it("inserts the chosen character AT THE CARET, not at the end", async () => {
    render(composer());
    const box = screen.getByLabelText("Message") as HTMLTextAreaElement;
    await userEvent.type(box, "hello world");
    // Put the caret between "hello" and " world", as a person would by clicking.
    box.setSelectionRange(5, 5);

    await userEvent.click(screen.getByLabelText("Insert emoji"));
    await userEvent.click(screen.getByLabelText("Insert 🎉"));

    await waitFor(() => expect(box.value).toBe("hello🎉 world"));
    // Focus and caret come back to the box, so typing continues where it was.
    await waitFor(() => expect(document.activeElement).toBe(box));
    expect(box.selectionStart).toBe(5 + "🎉".length);
  });

  it("sends the composed text, emoji and all", async () => {
    const onSend = vi.fn();
    render(composer({ onSend }));
    await userEvent.type(screen.getByLabelText("Message"), "ship it ");
    await userEvent.click(screen.getByLabelText("Insert emoji"));
    await userEvent.click(screen.getByLabelText("Insert 🚀"));
    await waitFor(() =>
      expect((screen.getByLabelText("Message") as HTMLTextAreaElement).value).toBe(
        "ship it 🚀",
      ),
    );
    await userEvent.click(screen.getByText("Send"));
    expect(onSend).toHaveBeenCalledWith("ship it 🚀");
  });

  it("is reachable and dismissable from the keyboard alone", async () => {
    render(composer());
    const trigger = screen.getByLabelText("Insert emoji");
    await userEvent.click(trigger);
    // Opening lands focus INSIDE the grid, so there is somewhere to arrow from.
    const grid = screen.getByRole("menu");
    expect(grid.contains(document.activeElement)).toBe(true);
    const first = document.activeElement;

    // Arrows walk the grid: right by one, down by a row, and back again.
    await userEvent.keyboard("{ArrowRight}");
    expect(document.activeElement).not.toBe(first);
    await userEvent.keyboard("{ArrowLeft}");
    expect(document.activeElement).toBe(first);
    await userEvent.keyboard("{ArrowDown}");
    expect(document.activeElement).not.toBe(first);
    await userEvent.keyboard("{ArrowUp}");
    expect(document.activeElement).toBe(first);
    // ArrowUp on the top row has nowhere to go and must not wrap or crash.
    await userEvent.keyboard("{ArrowUp}");
    expect(document.activeElement).toBe(first);

    // Enter picks the focused emoji…
    await userEvent.keyboard("{Enter}");
    await waitFor(() =>
      expect((screen.getByLabelText("Message") as HTMLTextAreaElement).value).not.toBe(""),
    );
    expect(screen.queryByRole("menu")).toBeNull();

    // …and Escape closes without picking, handing focus back to the trigger.
    await userEvent.click(trigger);
    await userEvent.keyboard("{Escape}");
    expect(screen.queryByRole("menu")).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });
});

describe("GIPHY picker (AC-2/AC-4)", () => {
  it("searches and posts the chosen GIF's URL as a message", async () => {
    const onSend = vi.fn();
    render(composer({ onSend, giphyKey: KEY }));

    await userEvent.click(screen.getByLabelText("Post a GIF"));
    // It opens on trending — an empty query — so there is something to pick at
    // once rather than an empty grid waiting to be typed into.
    await waitFor(() => expect(vi.mocked(searchGifs)).toHaveBeenCalled());
    expect(vi.mocked(searchGifs).mock.calls[0][0]).toBe(KEY);

    const option = await screen.findByLabelText("Post party parrot");
    expect(option.querySelector("img")?.getAttribute("src")).toBe(GIF_PREVIEW);

    await userEvent.click(option);
    // AC-2: the URL IS the message. Nothing is uploaded (NG-1).
    expect(onSend).toHaveBeenCalledWith(GIF_FULL);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("renders GIPHY's attribution mark with the results (AC-4)", async () => {
    render(composer({ giphyKey: KEY }));
    await userEvent.click(screen.getByLabelText("Post a GIF"));
    const mark = await screen.findByText("Powered by GIPHY");
    expect(mark.getAttribute("href")).toBe("https://giphy.com");
  });

  it("closes on Escape, handing focus back to its trigger", async () => {
    render(composer({ giphyKey: KEY }));
    const trigger = screen.getByLabelText("Post a GIF");
    await userEvent.click(trigger);
    expect(await screen.findByRole("dialog")).toBeTruthy();
    await userEvent.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });

  it("says so, rather than hanging, when GIPHY cannot be reached", async () => {
    vi.mocked(searchGifs).mockRejectedValueOnce(new Error("network down"));
    render(composer({ giphyKey: KEY }));
    await userEvent.click(screen.getByLabelText("Post a GIF"));
    expect(await screen.findByText("GIPHY could not be reached.")).toBeTruthy();
  });

  it("forgets the last search when it closes, so a reopen is not stale", async () => {
    render(composer({ giphyKey: KEY }));
    const trigger = screen.getByLabelText("Post a GIF");
    await userEvent.click(trigger);
    await userEvent.type(await screen.findByLabelText("Search GIPHY"), "parrot");
    await waitFor(() =>
      expect(vi.mocked(searchGifs).mock.calls.at(-1)?.[1]).toBe("parrot"),
    );
    await userEvent.keyboard("{Escape}");

    await userEvent.click(trigger);
    // Reopening searches trending again, from an empty box — rather than
    // repainting the previous query's results until the new request lands.
    expect((screen.getByLabelText("Search GIPHY") as HTMLInputElement).value).toBe("");
    await waitFor(() => expect(vi.mocked(searchGifs).mock.calls.at(-1)?.[1]).toBe(""));
  });
});

describe("composer tab order", () => {
  it("puts the message box before the pickers, whatever they look like", async () => {
    render(composer({ giphyKey: KEY }));
    const box = screen.getByLabelText("Message");
    // The primary control is what tabbing into the composer should reach first,
    // so the pickers follow it in the DOM and are moved back visually with CSS.
    const after = [
      screen.getByLabelText("Insert emoji"),
      screen.getByLabelText("Post a GIF"),
      screen.getByText("Send"),
    ];
    for (const later of after) {
      expect(box.compareDocumentPosition(later) & Node.DOCUMENT_POSITION_FOLLOWING).toBe(
        Node.DOCUMENT_POSITION_FOLLOWING,
      );
    }
  });
});

describe("a posted GIF in the log (AC-2)", () => {
  const gifMessage: ChatViewMessage = {
    id: "m-gif",
    authorId: "u-me",
    authorName: "me",
    body: GIF_FULL,
    createdAt: "2026-08-09T10:00:00Z",
    // Team chat renders every body as chat-flavoured markdown; a GIF must win
    // over that, or it renders as an autolink instead of a picture.
    markdown: "chat",
  };

  it("renders the URL as a bounded image that links to the original", () => {
    const { container } = render(composer({ messages: [gifMessage], currentUserId: "u-me" }));
    const img = container.querySelector("img.chat-gif") as HTMLImageElement;
    expect(img).toBeTruthy();
    expect(img.getAttribute("src")).toBe(GIF_FULL);
    // Click for full: the anchor around it opens the original in a new tab.
    const link = img.closest("a") as HTMLAnchorElement;
    expect(link.getAttribute("href")).toBe(GIF_FULL);
    expect(link.getAttribute("target")).toBe("_blank");
    expect(link.getAttribute("rel")).toContain("noreferrer");
    // AC-4: the attribution travels with the GIF, not only with the picker.
    expect(within(container.querySelector(".chat-body.gif")!).getByText("via GIPHY")).toBeTruthy();
    // The raw URL is not also printed as text beside the picture.
    expect(screen.queryByText(GIF_FULL)).toBeNull();
  });

  it("leaves any other URL as ordinary text", () => {
    const { container } = render(
      composer({
        messages: [{ ...gifMessage, id: "m2", body: "https://evil.example.com/x.gif" }],
      }),
    );
    expect(container.querySelector("img.chat-gif")).toBeNull();
  });

  it("keeps a deleted GIF message as its placeholder", () => {
    const { container } = render(
      composer({ messages: [{ ...gifMessage, deleted: true, body: "message deleted" }] }),
    );
    expect(container.querySelector("img.chat-gif")).toBeNull();
    expect(screen.getByText("message deleted")).toBeTruthy();
  });
});

// AC-3, the state every open-source deployment is in until its operator brings
// a key: no GIF button at all, and everything else untouched.
describe("no Giphy key configured (AC-3)", () => {
  it.each([
    ["absent", {}],
    ["null", { giphyKey: null }],
    ["empty", { giphyKey: "" }],
  ])("renders no GIF button when the key is %s", async (_label, props) => {
    render(composer(props));
    expect(screen.queryByLabelText("Post a GIF")).toBeNull();
    // Not a disabled one either — absent means absent.
    expect(screen.queryByText("GIF")).toBeNull();
    expect(vi.mocked(searchGifs)).not.toHaveBeenCalled();

    // The emoji picker is untouched: it needs no service.
    await userEvent.click(screen.getByLabelText("Insert emoji"));
    await userEvent.click(screen.getByLabelText("Insert 🎉"));
    await waitFor(() =>
      expect((screen.getByLabelText("Message") as HTMLTextAreaElement).value).toBe("🎉"),
    );
  });
});
