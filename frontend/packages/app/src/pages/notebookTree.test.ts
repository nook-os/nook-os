import { describe, expect, it } from "vitest";
import type { UserNoteFolder, UserNoteSummary } from "@nookos/api";
import {
  apiErrorMessage,
  buildFolderUpdate,
  buildNoteUpdate,
  buildTree,
} from "./notebookTree";

// Minimal fixtures — rows only need the fields the helpers touch.
const folder = (id: string, name: string, parent_id: string | null = null): UserNoteFolder =>
  ({ id, name, parent_id }) as unknown as UserNoteFolder;

const note = (id: string, title: string, folder_id: string | null = null): UserNoteSummary =>
  ({ id, title, folder_id, path: "" }) as unknown as UserNoteSummary;

describe("buildTree", () => {
  it("nests folders and lists notes under their folder", () => {
    const tree = buildTree(
      [folder("work", "Work"), folder("ideas", "Ideas", "work")],
      [note("n1", "root note"), note("n2", "work note", "work"), note("n3", "idea", "ideas")],
    );

    // Root: the "Work" folder and the single root note.
    expect(tree.folders.map((f) => f.folder.id)).toEqual(["work"]);
    expect(tree.rootNotes.map((n) => n.id)).toEqual(["n1"]);

    const work = tree.folders[0];
    expect(work.notes.map((n) => n.id)).toEqual(["n2"]);
    expect(work.children.map((f) => f.folder.id)).toEqual(["ideas"]);
    expect(work.children[0].notes.map((n) => n.id)).toEqual(["n3"]);
  });

  it("treats a folder with a missing parent as a root folder (orphan-to-root)", () => {
    // "ghost" is not in the folder list, so "orphan" rises to the root rather
    // than disappearing.
    const tree = buildTree([folder("orphan", "Orphan", "ghost")], []);
    expect(tree.folders.map((f) => f.folder.id)).toEqual(["orphan"]);
  });

  it("lists a note whose folder is missing at the root rather than dropping it", () => {
    const tree = buildTree([], [note("n1", "stray", "ghost")]);
    expect(tree.rootNotes.map((n) => n.id)).toEqual(["n1"]);
  });

  it("orders folders by name and notes by title, case-insensitively", () => {
    const tree = buildTree(
      [folder("b", "beta"), folder("a", "Alpha")],
      [note("z", "zebra"), note("a", "apple")],
    );
    expect(tree.folders.map((f) => f.folder.name)).toEqual(["Alpha", "beta"]);
    expect(tree.rootNotes.map((n) => n.title)).toEqual(["apple", "zebra"]);
  });
});

describe("buildNoteUpdate (tri-state folder_id — AC-4)", () => {
  it("move-to-root sends an EXPLICIT null folder_id", () => {
    const body = buildNoteUpdate({ move: null });
    expect("folder_id" in body).toBe(true);
    expect(body.folder_id).toBeNull();
  });

  it("move-into-folder sends the id", () => {
    const body = buildNoteUpdate({ move: "work" });
    expect(body.folder_id).toBe("work");
  });

  it("a leave-unchanged edit OMITS the folder_id key entirely", () => {
    const body = buildNoteUpdate({ title: "renamed" });
    expect("folder_id" in body).toBe(false);
    expect(body.title).toBe("renamed");
    // And it truly serializes without the key — the wire contract, not just the
    // in-memory shape.
    expect(JSON.stringify(body)).toBe('{"title":"renamed"}');
  });

  it("carries title and content when supplied", () => {
    const body = buildNoteUpdate({ title: "t", content_md: "# hi" });
    expect(body).toEqual({ title: "t", content_md: "# hi" });
  });
});

describe("buildFolderUpdate (tri-state parent_id — AC-4)", () => {
  it("move-to-root sends an EXPLICIT null parent_id", () => {
    const body = buildFolderUpdate({ move: null });
    expect("parent_id" in body).toBe(true);
    expect(body.parent_id).toBeNull();
  });

  it("a rename-only edit OMITS the parent_id key", () => {
    const body = buildFolderUpdate({ name: "Renamed" });
    expect("parent_id" in body).toBe(false);
    expect(JSON.stringify(body)).toBe('{"name":"Renamed"}');
  });
});

describe("apiErrorMessage", () => {
  it("extracts the control plane's { error } message (the two 400s)", () => {
    expect(apiErrorMessage({ error: "a note title cannot be blank" })).toBe(
      "a note title cannot be blank",
    );
    expect(
      apiErrorMessage({ error: "that move would put a folder inside its own subtree" }),
    ).toBe("that move would put a folder inside its own subtree");
  });

  it("falls back when there is no usable message", () => {
    expect(apiErrorMessage(undefined, "nope")).toBe("nope");
    expect(apiErrorMessage({ error: "  " }, "nope")).toBe("nope");
  });
});
