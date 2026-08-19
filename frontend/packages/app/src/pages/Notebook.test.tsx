// MAIN-634: the notebook tree's right-click menus, which replaced the hover
// icon buttons. What is worth asserting is not that a menu appears but WHICH
// menu appears — the folder's, the note's, or the tree's — because a 300px pane
// that offers "Delete folder" over a note is worse than no menu at all.
//
// MAIN-635: the same rows in place — the focus a keyboard user needs, F2/Delete,
// and the rename that happens on the row instead of in a dialog. The fixtures
// are MUTABLE from here on: creating an item has to make it appear in the tree,
// because "the new row is in rename mode" is the whole of AC-8.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";
import { ContextMenuProvider } from "../contextMenu";

interface Folder {
  id: string;
  name: string;
  parent_id: string | null;
}
interface Note {
  id: string;
  title: string;
  folder_id: string | null;
}

const db = vi.hoisted(() => ({
  folders: [] as Folder[],
  notes: [] as Note[],
  seq: 0,
}));

// A COPY of each list, as a real fetch would hand back: returning the fixture
// array itself means a create mutates the very object react-query already
// holds, so its structural sharing sees no change and nothing re-renders.
const get = vi.hoisted(() =>
  vi.fn(async (path: string) => {
    if (path === "/api/v1/notebook/folders") return { data: [...db.folders] };
    if (path === "/api/v1/notebook/notes") return { data: [...db.notes] };
    if (path === "/api/v1/notebook/notes/{id}")
      return { data: { id: "n1", title: "Inside note", content_md: "body" } };
    return { data: [] };
  }),
);
// Create writes to the fixture, so the refetch the page triggers actually
// returns the new row.
const post = vi.hoisted(() =>
  vi.fn(async (path: string, opts: { body: Record<string, string | null> }) => {
    db.seq += 1;
    if (path === "/api/v1/notebook/notes") {
      const note: Note = {
        id: `new-n${db.seq}`,
        title: String(opts.body.title),
        folder_id: opts.body.folder_id ?? null,
      };
      db.notes.push(note);
      return { data: note, response: { ok: true } };
    }
    const folder: Folder = {
      id: `new-f${db.seq}`,
      name: String(opts.body.name),
      parent_id: opts.body.parent_id ?? null,
    };
    db.folders.push(folder);
    return { data: folder, response: { ok: true } };
  }),
);
const patch = vi.hoisted(() => vi.fn(async () => ({ data: {}, response: { ok: true } })));
const del = vi.hoisted(() => vi.fn(async () => ({ data: {}, response: { ok: true } })));

const askText = vi.hoisted(() => vi.fn(async () => "typed"));
const askChoice = vi.hoisted(() => vi.fn(async () => "f1"));
const askConfirm = vi.hoisted(() => vi.fn(async () => true));

vi.mock("@nookos/api", () => ({
  api: { GET: get, POST: post, PATCH: patch, DELETE: del, PUT: vi.fn() },
}));
vi.mock("../dialogs", () => ({ askChoice, askConfirm, askText }));

import { Notebook } from "./Notebook";

function renderNotebook() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <ContextMenuProvider>
          <Notebook />
        </ContextMenuProvider>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

/** The row div itself — the label is a span inside it, and the row is what
 *  carries the class and the handlers. Async because the tree is drawn from two
 *  queries, so the first look at any row has to wait for them. */
async function row(label: string): Promise<HTMLElement> {
  const el = (await screen.findByText(label)).closest(".notebook-row");
  if (!el) throw new Error(`no row for ${label}`);
  return el as HTMLElement;
}

/** Focus a row the way Tab does, then press a key on it. */
async function pressOn(label: string, key: string): Promise<HTMLElement> {
  const el = await row(label);
  el.focus();
  fireEvent.keyDown(el, { key });
  return el;
}

/** The open rename input, or null. There is at most one — the tree owns the
 *  state, so a second row cannot be editing at the same time. */
function renameInput(): HTMLInputElement | null {
  return screen.queryByLabelText("Rename") as HTMLInputElement | null;
}

/** The open menu's item labels, in render order. */
function menuLabels(): string[] {
  return screen.queryAllByRole("menuitem").map((el) => el.textContent ?? "");
}

async function openMenuOn(el: HTMLElement) {
  fireEvent.contextMenu(el);
  await screen.findByRole("menu");
}

beforeEach(() => {
  localStorage.setItem("nook.notesMcpBannerDismissed", "1");
  db.folders = [
    { id: "f1", name: "Work", parent_id: null },
    { id: "f2", name: "Zebra", parent_id: null },
  ];
  db.notes = [
    { id: "n1", title: "Inside note", folder_id: "f1" },
    { id: "n2", title: "Root note", folder_id: null },
  ];
  db.seq = 0;
});
afterEach(() => {
  cleanup();
  localStorage.clear();
});

describe("the notebook tree's context menus", () => {
  it("offers the folder's own actions, in order, on a folder row (AC-1)", async () => {
    renderNotebook();
    await openMenuOn(await row("Work"));

    expect(menuLabels()).toEqual([
      "New note",
      "New sub-folder",
      "Rename",
      "Move…",
      "Delete folder",
    ]);
    // The two groups the separators create are part of the contract: creating
    // must not sit next to deleting.
    expect(screen.getAllByRole("separator")).toHaveLength(2);
    expect(screen.getByText("Delete folder").closest("button")?.className).toContain("danger");
  });

  it("offers the note's actions on a note row (AC-2)", async () => {
    renderNotebook();
    await openMenuOn(await row("Root note"));

    expect(menuLabels()).toEqual(["Rename", "Move…", "Delete"]);
    expect(screen.getAllByRole("separator")).toHaveLength(1);
    expect(screen.getByText("Delete").closest("button")?.className).toContain("danger");
  });

  it("offers root creation on the empty space below the rows (AC-3)", async () => {
    const { container } = renderNotebook();
    await screen.findByText("Work");
    await openMenuOn(container.querySelector(".notebook-tree") as HTMLElement);

    expect(menuLabels()).toEqual(["New note", "New folder"]);
  });

  it("creates at the ROOT from the tree's own menu, like the header buttons (AC-3)", async () => {
    const { container } = renderNotebook();
    await screen.findByText("Work");
    await openMenuOn(container.querySelector(".notebook-tree") as HTMLElement);
    fireEvent.click(screen.getByText("New folder"));

    await waitFor(() =>
      expect(post).toHaveBeenCalledWith("/api/v1/notebook/folders", {
        body: { name: "New folder", parent_id: null },
      }),
    );
  });

  it("gives a nested note the NOTE menu, never the folder it sits in (AC-4)", async () => {
    renderNotebook();
    fireEvent.click(await row("Work")); // expand
    await openMenuOn(await row("Inside note"));

    expect(menuLabels()).toEqual(["Rename", "Move…", "Delete"]);
    // The enclosing folder's items are the ones that must not leak through.
    expect(screen.queryByText("New sub-folder")).toBeNull();
    expect(screen.queryByText("Delete folder")).toBeNull();
  });

  it("has no hover action buttons left on either row type (AC-5)", async () => {
    const { container } = renderNotebook();
    fireEvent.click(await screen.findByText("Work"));
    await screen.findByText("Inside note");

    expect(container.querySelector(".notebook-row-actions")).toBeNull();
    // The panel header keeps its two, which are the only way to create at the
    // root without finding empty space.
    expect(screen.getByTitle("new note at root")).toBeTruthy();
    expect(screen.getByTitle("new folder at root")).toBeTruthy();
  });

  it("marks the row the open menu is about, and only while it is open (AC-6)", async () => {
    renderNotebook();
    const work = await row("Work");
    await openMenuOn(work);

    expect(work.className).toContain("ctx-target");
    expect((await row("Zebra")).className).not.toContain("ctx-target");

    fireEvent.keyDown(window, { key: "Escape" });
    await waitFor(async () =>
      expect((await row("Work")).className).not.toContain("ctx-target"),
    );
  });

  it("moves the mark to the row you right-click next (AC-6)", async () => {
    renderNotebook();
    await openMenuOn(await row("Work"));
    // A real right-click presses first; that press is what drops the old mark.
    fireEvent.mouseDown(await row("Zebra"), { button: 2 });
    await openMenuOn(await row("Zebra"));

    expect((await row("Zebra")).className).toContain("ctx-target");
    expect((await row("Work")).className).not.toContain("ctx-target");
  });

  it("does not open the note you right-click (AC-7)", async () => {
    renderNotebook();
    await openMenuOn(await row("Root note"));

    // The right pane still says nothing is selected, and no note was fetched.
    expect(screen.getByText("No note selected")).toBeTruthy();
    expect(get).not.toHaveBeenCalledWith(
      "/api/v1/notebook/notes/{id}",
      expect.anything(),
    );
  });

  it("suppresses the browser's own menu everywhere in the tree, with no opt-out (AC-8)", async () => {
    const { container } = renderNotebook();
    await screen.findByText("Work");

    for (const el of [
      await row("Work"),
      await row("Root note"),
      container.querySelector(".notebook-tree") as HTMLElement,
    ]) {
      const ev = new MouseEvent("contextmenu", { bubbles: true, cancelable: true });
      el.dispatchEvent(ev);
      expect(ev.defaultPrevented).toBe(true);
    }
    // The opt-out attribute would hand the gesture back to a legacy handler
    // there is none of here.
    expect(container.querySelector("[data-ctxmenu-native]")).toBeNull();
  });

  it("still routes Delete through the existing confirm dialog (AC-9/NG-1)", async () => {
    renderNotebook();
    await openMenuOn(await row("Root note"));
    fireEvent.click(screen.getByText("Delete"));

    await waitFor(() => expect(askConfirm).toHaveBeenCalled());
    await waitFor(() =>
      expect(del).toHaveBeenCalledWith("/api/v1/notebook/notes/{id}", {
        params: { path: { id: "n2" } },
      }),
    );
  });

  it("surfaces a rejected call in the inline banner, as the buttons did (AC-9)", async () => {
    del.mockResolvedValueOnce({
      error: { error: "note not found" },
      response: { ok: false },
    } as never);
    renderNotebook();
    await openMenuOn(await row("Root note"));
    fireEvent.click(screen.getByText("Delete"));

    const banner = await screen.findByRole("alert");
    expect(banner.textContent).toContain("note not found");
  });

  it("keeps Move on the askChoice picker (NG-1)", async () => {
    renderNotebook();
    await openMenuOn(await row("Root note"));
    fireEvent.click(screen.getByText("Move…"));

    await waitFor(() => expect(askChoice).toHaveBeenCalled());
    await waitFor(() =>
      expect(patch).toHaveBeenCalledWith("/api/v1/notebook/notes/{id}", {
        params: { path: { id: "n2" } },
        body: { folder_id: "f1" },
      }),
    );
  });
});

describe("the notebook tree's inline rename (MAIN-635)", () => {
  it("opens an input, pre-filled and selected, on F2 over a folder (AC-3)", async () => {
    renderNotebook();
    await pressOn("Work", "F2");

    const input = renameInput();
    expect(input?.value).toBe("Work");
    expect(input?.maxLength).toBe(200);
    expect([input?.selectionStart, input?.selectionEnd]).toEqual([0, 4]);
  });

  it("opens the same input on F2 over a note (AC-3)", async () => {
    renderNotebook();
    await pressOn("Root note", "F2");

    expect(renameInput()?.value).toBe("Root note");
  });

  it("commits on Enter, and on blur, with the PATCH the dialog used to send (AC-4)", async () => {
    renderNotebook();
    await pressOn("Work", "F2");
    fireEvent.change(renameInput() as HTMLInputElement, { target: { value: "Admin" } });
    fireEvent.keyDown(renameInput() as HTMLInputElement, { key: "Enter" });

    await waitFor(() =>
      expect(patch).toHaveBeenCalledWith("/api/v1/notebook/folders/{id}", {
        params: { path: { id: "f1" } },
        body: { name: "Admin" },
      }),
    );
    await waitFor(() => expect(renameInput()).toBeNull());

    await pressOn("Root note", "F2");
    fireEvent.change(renameInput() as HTMLInputElement, { target: { value: "Blurred" } });
    fireEvent.blur(renameInput() as HTMLInputElement);

    await waitFor(() =>
      expect(patch).toHaveBeenCalledWith("/api/v1/notebook/notes/{id}", {
        params: { path: { id: "n2" } },
        body: { title: "Blurred" },
      }),
    );
  });

  it("reverts on Escape and sends nothing (AC-4)", async () => {
    renderNotebook();
    await pressOn("Work", "F2");
    fireEvent.change(renameInput() as HTMLInputElement, { target: { value: "Discarded" } });
    fireEvent.keyDown(renameInput() as HTMLInputElement, { key: "Escape" });

    await waitFor(() => expect(renameInput()).toBeNull());
    expect((await row("Work")).textContent).toContain("Work");
    expect(patch).not.toHaveBeenCalled();
  });

  it.each([
    ["blank", ""],
    ["whitespace-only", "   "],
  ])("rejects a %s name locally and reverts (AC-5)", async (_label, value) => {
    renderNotebook();
    await pressOn("Work", "F2");
    fireEvent.change(renameInput() as HTMLInputElement, { target: { value } });
    fireEvent.blur(renameInput() as HTMLInputElement);

    await waitFor(() => expect(renameInput()).toBeNull());
    expect((await row("Work")).textContent).toContain("Work");
    expect(patch).not.toHaveBeenCalled();
  });

  it("puts a rejected rename in the banner and leaves the old label (AC-9)", async () => {
    patch.mockResolvedValueOnce({
      error: { error: "name already taken" },
      response: { ok: false },
    } as never);
    renderNotebook();
    await pressOn("Work", "F2");
    fireEvent.change(renameInput() as HTMLInputElement, { target: { value: "Zebra" } });
    fireEvent.keyDown(renameInput() as HTMLInputElement, { key: "Enter" });

    const banner = await screen.findByRole("alert");
    expect(banner.textContent).toContain("name already taken");
    expect((await row("Work")).textContent).toContain("Work");
  });

  it("enters inline rename from the context menu instead of a dialog (AC-7/AC-10)", async () => {
    renderNotebook();
    await openMenuOn(await row("Root note"));
    fireEvent.click(screen.getByText("Rename"));

    await waitFor(() => expect(renameInput()?.value).toBe("Root note"));
    expect(askText).not.toHaveBeenCalled();
  });

  it("creates a note under its default name, in rename mode, and keeps it on Escape (AC-8)", async () => {
    renderNotebook();
    await screen.findByText("Work");
    fireEvent.click(screen.getByTitle("new note at root"));

    await waitFor(() => expect(renameInput()?.value).toBe("Untitled"));
    expect(post).toHaveBeenCalledTimes(1);
    expect(post).toHaveBeenCalledWith("/api/v1/notebook/notes", {
      body: { title: "Untitled", folder_id: null },
    });
    expect(askText).not.toHaveBeenCalled();

    fireEvent.keyDown(renameInput() as HTMLInputElement, { key: "Escape" });
    await waitFor(() => expect(renameInput()).toBeNull());
    expect(await row("Untitled")).toBeTruthy();
    expect(del).not.toHaveBeenCalled();
  });

  it("creates a folder's new note inside it, revealed and renaming (AC-8)", async () => {
    renderNotebook();
    await openMenuOn(await row("Work"));
    fireEvent.click(screen.getByText("New note"));

    await waitFor(() =>
      expect(post).toHaveBeenCalledWith("/api/v1/notebook/notes", {
        body: { title: "Untitled", folder_id: "f1" },
      }),
    );
    // Revealed: the folder was collapsed, so its sibling note proves it opened.
    await screen.findByText("Inside note");
    await waitFor(() => expect(renameInput()?.value).toBe("Untitled"));
  });

  it("creates a folder under its default name, in rename mode (AC-8)", async () => {
    renderNotebook();
    await screen.findByText("Work");
    fireEvent.click(screen.getByTitle("new folder at root"));

    await waitFor(() => expect(renameInput()?.value).toBe("New folder"));
    expect(post).toHaveBeenCalledTimes(1);
  });
});

describe("the notebook tree's row focus and keys (MAIN-635)", () => {
  it("makes every row tabbable and rings the focused one (AC-1/AC-2)", async () => {
    renderNotebook();
    const work = await pressOn("Work", "Tab");

    expect(work.getAttribute("tabindex")).toBe("0");
    expect((await row("Root note")).getAttribute("tabindex")).toBe("0");
    expect(work.className).toContain("focused");
    expect((await row("Root note")).className).not.toContain("focused");
  });

  it("focuses a folder on click without changing the open note (AC-1)", async () => {
    renderNotebook();
    fireEvent.click(await row("Root note"));

    // The note query fired for the clicked note; the editor pane is on it.
    await waitFor(() =>
      expect(get).toHaveBeenCalledWith("/api/v1/notebook/notes/{id}", {
        params: { path: { id: "n2" } },
      }),
    );
    get.mockClear();

    const work = await row("Work");
    fireEvent.click(work);

    expect(work.className).toContain("focused");
    expect((await row("Root note")).className).toContain("selected");
    expect(get).not.toHaveBeenCalledWith(
      "/api/v1/notebook/notes/{id}",
      expect.anything(),
    );
  });

  it("opens the existing confirm dialog on Delete over a note (AC-6)", async () => {
    renderNotebook();
    await pressOn("Root note", "Delete");

    await waitFor(() => expect(askConfirm).toHaveBeenCalled());
    await waitFor(() =>
      expect(del).toHaveBeenCalledWith("/api/v1/notebook/notes/{id}", {
        params: { path: { id: "n2" } },
      }),
    );
  });

  it("opens the folder's confirm dialog on Delete over a folder (AC-6)", async () => {
    renderNotebook();
    await pressOn("Work", "Delete");

    await waitFor(() =>
      expect(askConfirm).toHaveBeenCalledWith(
        expect.objectContaining({ title: 'Delete folder "Work"?' }),
      ),
    );
    await waitFor(() =>
      expect(del).toHaveBeenCalledWith("/api/v1/notebook/folders/{id}", {
        params: { path: { id: "f1" } },
      }),
    );
  });

  it("leaves Delete to the input while a rename is open (AC-6)", async () => {
    renderNotebook();
    await pressOn("Work", "F2");
    fireEvent.keyDown(renameInput() as HTMLInputElement, { key: "Delete" });

    expect(askConfirm).not.toHaveBeenCalled();
    expect(renameInput()).not.toBeNull();
  });
});
