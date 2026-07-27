// Pure helpers for the personal notebook (MAIN-101). Kept out of the page
// component so the two things most worth testing without a live API — building
// the nested tree from the server's FLAT lists, and the tri-state PATCH payload
// footgun — are plain functions with plain inputs (mirrors backlogSelection.ts).
import type {
  UpdateUserNote,
  UpdateUserNoteFolder,
  UserNoteFolder,
  UserNoteFolderId,
  UserNoteSummary,
} from "@nookos/api";

/** One node of the client-built folder tree: the folder, its sub-folders, and
 *  the notes that live directly inside it. */
export interface FolderNode {
  folder: UserNoteFolder;
  children: FolderNode[];
  notes: UserNoteSummary[];
}

/** The whole tree: root-level folders and the notes that sit at the root
 *  (folder_id null). */
export interface NotebookTree {
  folders: FolderNode[];
  rootNotes: UserNoteSummary[];
}

/**
 * Build the nested tree from the two flat lists the API returns
 * (`GET /folders`, `GET /notes`) — the backend never nests for us (AC-2).
 *
 * Two robustness rules make this total for any input the server can hand back:
 *   - A folder whose `parent_id` points at a folder we did not receive is
 *     treated as a root folder (orphan-to-root) rather than vanishing.
 *   - A note whose `folder_id` is null OR points at a missing folder is listed
 *     at the root, so a note is never silently dropped.
 *
 * Folders are ordered by name and notes by title, case-insensitively, so the
 * render is deterministic regardless of the order rows arrive in.
 */
export function buildTree(
  folders: UserNoteFolder[],
  notes: UserNoteSummary[],
): NotebookTree {
  const nodes = new Map<UserNoteFolderId, FolderNode>();
  for (const folder of folders) {
    nodes.set(folder.id, { folder, children: [], notes: [] });
  }

  // Notes into their folder bucket; anything without a live folder → root.
  const rootNotes: UserNoteSummary[] = [];
  for (const note of notes) {
    const parent = note.folder_id ? nodes.get(note.folder_id) : undefined;
    if (parent) parent.notes.push(note);
    else rootNotes.push(note);
  }

  // Folders under their parent; a missing/absent parent → root.
  const roots: FolderNode[] = [];
  for (const node of nodes.values()) {
    const parentId = node.folder.parent_id;
    const parent = parentId ? nodes.get(parentId) : undefined;
    if (parent) parent.children.push(node);
    else roots.push(node);
  }

  const byName = (a: FolderNode, b: FolderNode) =>
    a.folder.name.localeCompare(b.folder.name, undefined, { sensitivity: "base" });
  const byTitle = (a: UserNoteSummary, b: UserNoteSummary) =>
    a.title.localeCompare(b.title, undefined, { sensitivity: "base" });

  for (const node of nodes.values()) {
    node.children.sort(byName);
    node.notes.sort(byTitle);
  }
  roots.sort(byName);
  rootNotes.sort(byTitle);

  return { folders: roots, rootNotes };
}

/**
 * A move instruction for a note or folder. Its ABSENCE (the field left
 * `undefined` on the edit object) means "leave the parent unchanged"; a present
 * value of `null` means "move to the root", and an id means "move under that
 * folder". This is the tri-state the backend implements as
 * None / Some(None) / Some(Some) (AC-4).
 */
export type MoveTarget = UserNoteFolderId | null;

/** The editable fields of a note plus an OPTIONAL move. Omit `move` entirely to
 *  leave the folder where it is; pass `{ move: null }` to send it to root. */
export interface NoteEdit {
  title?: string;
  content_md?: string;
  move?: MoveTarget;
}

/**
 * Build the PATCH body for a note — the load-bearing footgun (AC-4).
 *
 * `folder_id` is written ONLY when the caller supplied a `move`, and then it is
 * written literally — `null` to move to root, an id to move under a folder.
 * When no move was asked for, the key is absent from the returned object, which
 * the backend reads as "leave unchanged". Omitting the key and sending
 * `folder_id: null` mean opposite things, so we never let an accidental
 * `undefined` stand in for the deliberate `null`.
 */
export function buildNoteUpdate(edit: NoteEdit): UpdateUserNote {
  const body: UpdateUserNote = {};
  if (edit.title !== undefined) body.title = edit.title;
  if (edit.content_md !== undefined) body.content_md = edit.content_md;
  if ("move" in edit) body.folder_id = edit.move ?? null;
  return body;
}

/** The editable fields of a folder plus an OPTIONAL move (same tri-state as a
 *  note's `folder_id`, but the key is `parent_id`). */
export interface FolderEdit {
  name?: string;
  move?: MoveTarget;
}

/** Build the PATCH body for a folder. `parent_id` is the folder twin of a
 *  note's `folder_id` — present-and-null moves to root, absent leaves it. */
export function buildFolderUpdate(edit: FolderEdit): UpdateUserNoteFolder {
  const body: UpdateUserNoteFolder = {};
  if (edit.name !== undefined) body.name = edit.name;
  if ("move" in edit) body.parent_id = edit.move ?? null;
  return body;
}

/**
 * Pull the human-readable message out of an openapi-fetch error.
 *
 * The control plane renders every `ApiError` as `{ "error": message }`
 * (crates/nook-control/src/error.rs), so the two notebook 400s — a blank or
 * over-long title/name, and a folder-cycle move — arrive here as that shape.
 * We surface the message inline instead of letting it die in the console
 * (AC-3). Mirrors the `fail` helper in Team.tsx.
 */
export function apiErrorMessage(error: unknown, fallback = "Something went wrong"): string {
  if (error && typeof error === "object" && "error" in error) {
    const message = (error as { error: unknown }).error;
    if (typeof message === "string" && message.trim()) return message;
  }
  if (typeof error === "string" && error.trim()) return error;
  return fallback;
}
