// Picking a workspace from the `@` menu, and reading one back (MAIN-633).
//
// Against the REAL description editor — `EditableMarkdown` is what `TaskDetail`
// renders — because everything worth pinning here is an interaction between
// CodeMirror's keymap and a menu that never takes focus. A test over the menu
// component alone would assert that a list renders, which was never the risk.
//
// The rows are stubbed rather than fetched: which endpoint answers is
// `workspaceMentions`' business (asserted at the bottom), and a component test
// that also owned the transport would fail for two unrelated reasons.
import React from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { EditableMarkdown, Markdown, type MentionOption } from "@nookos/ui";
import { mentionLinks } from "./workspaceMentions";

afterEach(() => cleanup());

const WORKSPACES: MentionOption[] = [
  { workspace_id: "w1", name: "Nook API", slug: "nook-api" },
  { workspace_id: "w2", name: "Nook Web", slug: "nook-web" },
];

/** A source that answers from `WORKSPACES` with the same prefix rule the
 *  endpoint applies, so "narrows as you type" means the same thing here. */
function source(rows: MentionOption[] = WORKSPACES) {
  return {
    search: vi.fn(async (q: string) =>
      rows.filter(
        (w) =>
          w.slug.startsWith(q.toLowerCase()) ||
          w.name.toLowerCase().startsWith(q.toLowerCase()),
      ),
    ),
  };
}

/** The editor, already open, reporting what it would save. */
function editor(mentions: { search: (q: string) => Promise<MentionOption[]> }) {
  const saved: string[] = [];
  render(
    <EditableMarkdown
      value=""
      editing
      onSave={(next) => {
        saved.push(next);
      }}
      mentions={mentions}
    />,
  );
  const content = document.querySelector(".cm-content") as HTMLElement;
  return { content, saved };
}

const rows = () => screen.queryAllByRole("option").map((o) => o.textContent);

/** Type into the editor a character at a time, waiting for each to land.
 *
 *  CodeMirror reads the mutations its contenteditable receives asynchronously
 *  and re-syncs the DOM from its own state when it does, so a burst delivered
 *  faster than that read loses characters — reliably so under a loaded parallel
 *  run, which is where this first showed up. A person types slower than a
 *  `for` loop; waiting for each keystroke to appear is what makes this a test
 *  of the menu rather than of jsdom's scheduling. */
async function type(content: HTMLElement, text: string) {
  for (const ch of text) {
    const before = content.textContent ?? "";
    await userEvent.keyboard(ch);
    await waitFor(() => expect(content.textContent).toBe(before + ch));
  }
}

describe("the @ menu", () => {
  it("opens on @ and narrows as more is typed", async () => {
    const mentions = source();
    const { content } = editor(mentions);
    content.focus();

    await type(content, "see @");
    await waitFor(() => expect(screen.getByRole("listbox")).toBeTruthy());
    expect(rows()).toEqual(["@nook-apiNook API", "@nook-webNook Web"]);
    expect(mentions.search).toHaveBeenCalledWith("");

    await type(content, "nook-w");
    await waitFor(() => expect(rows()).toEqual(["@nook-webNook Web"]));
    expect(mentions.search).toHaveBeenLastCalledWith("nook-w");
  });

  it("says so when nothing matches, rather than showing nothing at all", async () => {
    const { content } = editor(source());
    content.focus();

    await type(content, "@zzz");
    // The menu is still THERE — an absent menu and a menu with no rows say very
    // different things, and only one of them is true (AC-6).
    await waitFor(() => expect(screen.getByRole("listbox")).toBeTruthy());
    expect(rows()).toEqual([]);
    expect(screen.getByText(/no workspace matches/)).toBeTruthy();
  });

  it("inserts the picked slug and closes", async () => {
    const { content } = editor(source());
    content.focus();

    await type(content, "wire @nook-w");
    await waitFor(() => expect(rows()).toHaveLength(1));
    await userEvent.keyboard("{Enter}");

    await waitFor(() => expect(screen.queryByRole("listbox")).toBeNull());
    expect(content.textContent).toBe("wire @nook-web ");
  });

  it("closes on Escape with the typed text exactly as it was", async () => {
    const { content } = editor(source());
    content.focus();

    await type(content, "wire @nook-w");
    await waitFor(() => expect(screen.getByRole("listbox")).toBeTruthy());
    await userEvent.keyboard("{Escape}");

    await waitFor(() => expect(screen.queryByRole("listbox")).toBeNull());
    expect(content.textContent).toBe("wire @nook-w");
    // And it STAYS closed for that `@` — a menu that sprang back on the next
    // keystroke would make Escape look broken.
    await type(content, "e");
    expect(screen.queryByRole("listbox")).toBeNull();
    expect(content.textContent).toBe("wire @nook-we");
  });

  // AC-3, and deliberately no `click`/`mouseDown` anywhere in it: the menu is
  // reachable with the caret alone, and the caret never leaves the editor.
  it("is driven entirely from the keyboard, and leaves focus in the editor", async () => {
    const { content } = editor(source());
    content.focus();

    await type(content, "@");
    await waitFor(() => expect(rows()).toHaveLength(2));
    expect(screen.getAllByRole("option")[0].getAttribute("aria-selected")).toBe("true");

    await userEvent.keyboard("{ArrowDown}");
    await waitFor(() =>
      expect(screen.getAllByRole("option")[1].getAttribute("aria-selected")).toBe("true"),
    );
    // Wrapping is what makes a two-item menu usable without looking: the end of
    // the list is the start of it.
    await userEvent.keyboard("{ArrowUp}{ArrowUp}");
    await waitFor(() =>
      expect(screen.getAllByRole("option")[1].getAttribute("aria-selected")).toBe("true"),
    );

    await userEvent.keyboard("{Enter}");
    await waitFor(() => expect(screen.queryByRole("listbox")).toBeNull());
    expect(content.textContent).toBe("@nook-web ");
    expect(content.contains(document.activeElement)).toBe(true);
  });

  it("leaves Enter alone when the menu has nothing to pick", async () => {
    const { content } = editor(source());
    content.focus();

    await type(content, "@zzz");
    await waitFor(() => expect(screen.getByText(/no workspace matches/)).toBeTruthy());
    await userEvent.keyboard("{Enter}");

    // A newline, not a swallowed keystroke: there was nothing to select.
    expect(content.textContent).toContain("@zzz");
    expect(document.querySelectorAll(".cm-line").length).toBe(2);
  });
});

describe("a saved description", () => {
  const body = "Wire @nook-web against @not-a-repo.";
  const links = mentionLinks([
    { workspace_id: "w2", name: "Nook Web", slug: "nook-web", git_remote_url: null },
  ]);

  it("renders a resolved slug as a link to its workspace", () => {
    render(<Markdown src={body} mentions={links} />);
    const link = screen.getByRole("link", { name: "@nook-web" });
    expect(link.getAttribute("href")).toBe("/workspaces/w2");
    // In-app, so it stays in this tab.
    expect(link.getAttribute("target")).toBeNull();
  });

  it("leaves an unresolved slug as plain text, never a broken link", () => {
    render(<Markdown src={body} mentions={links} />);
    expect(screen.queryByRole("link", { name: "@not-a-repo" })).toBeNull();
    expect(screen.getAllByRole("link")).toHaveLength(1);
    expect(document.body.textContent).toContain("@not-a-repo");
  });

  it("links nothing at all when the card resolved nothing", () => {
    render(<Markdown src={body} mentions={[]} />);
    expect(screen.queryAllByRole("link")).toHaveLength(0);
  });
});

describe("mentionLinks", () => {
  it("points each resolved reference at its workspace page", () => {
    expect(
      mentionLinks([
        { workspace_id: "w1", name: "Nook API", slug: "nook-api", git_remote_url: null },
      ]),
    ).toEqual([{ slug: "nook-api", href: "/workspaces/w1" }]);
  });

  it("is empty for a card with no references", () => {
    expect(mentionLinks(undefined)).toEqual([]);
  });
});
