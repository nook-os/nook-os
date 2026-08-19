// MAIN-634: the notebook tree's right-click menus, which replaced the hover
// icon buttons. What is worth asserting is not that a menu appears but WHICH
// menu appears — the folder's, the note's, or the tree's — because a 300px pane
// that offers "Delete folder" over a note is worse than no menu at all.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";
import { ContextMenuProvider } from "../contextMenu";

const FOLDERS = vi.hoisted(() => [
  { id: "f1", name: "Work", parent_id: null },
  { id: "f2", name: "Zebra", parent_id: null },
]);
const NOTES = vi.hoisted(() => [
  { id: "n1", title: "Inside note", folder_id: "f1" },
  { id: "n2", title: "Root note", folder_id: null },
]);

const get = vi.hoisted(() =>
  vi.fn(async (path: string) => {
    if (path === "/api/v1/notebook/folders") return { data: FOLDERS };
    if (path === "/api/v1/notebook/notes") return { data: NOTES };
    if (path === "/api/v1/notebook/notes/{id}")
      return { data: { id: "n1", title: "Inside note", content_md: "body" } };
    return { data: [] };
  }),
);
const post = vi.hoisted(() => vi.fn(async () => ({ data: { id: "new" }, response: { ok: true } })));
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
        body: { name: "typed", parent_id: null },
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

  it("keeps Rename on the askText dialog rather than editing in place (NG-1)", async () => {
    renderNotebook();
    await openMenuOn(await row("Work"));
    fireEvent.click(screen.getByText("Rename"));

    await waitFor(() => expect(askText).toHaveBeenCalled());
    await waitFor(() =>
      expect(patch).toHaveBeenCalledWith("/api/v1/notebook/folders/{id}", {
        params: { path: { id: "f1" } },
        body: { name: "typed" },
      }),
    );
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
