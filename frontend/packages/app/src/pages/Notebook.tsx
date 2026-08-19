// The personal notebook (MAIN-101). A person-global, cross-org store of folders
// and markdown notes — NOT workspace-scoped, so it deliberately ignores the
// selected-workspace context. Two panes: a client-built folder tree on the left
// (from the flat `GET /folders` + `GET /notes` lists), and the selected note in
// an EditableMarkdown on the right. All persistence is the caller's job here
// (AC-3): every write is an explicit api call followed by a react-query
// invalidation, so the tree reflects backend reparenting with no page reload.
import React, { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  AlertCircle,
  ChevronDown,
  ChevronRight,
  FilePlus,
  FileText,
  Folder,
  FolderOpen,
  FolderPlus,
  X,
} from "lucide-react";
import { api } from "@nookos/api";
import type { UserNoteFolder, UserNoteSummary } from "@nookos/api";
import { EditableMarkdown, Empty, Panel, SearchInput } from "@nookos/ui";
import { ContextMenuRegion, type ContextMenuItem } from "../contextMenu";
import { askChoice, askConfirm, askText } from "../dialogs";
import {
  apiErrorMessage,
  buildFolderUpdate,
  buildNoteUpdate,
  buildTree,
  type FolderNode,
} from "./notebookTree";
import { NotesMcpBanner } from "./NotesMcpBanner";

/** The most a note title / folder name may be — surfaced from MAIN-84's
 *  server-side `MAX_NAME_LEN`, so the input can cap and count before the API
 *  ever rejects a blank/too-long value. */
const MAX_NAME_LEN = 200;

const ROOT = "__root__";

/** Human-readable "Work / Ideas" path for every folder, for the move picker. */
function folderPathChoices(folders: UserNoteFolder[], exclude: Set<string>) {
  const byId = new Map(folders.map((f) => [f.id, f]));
  const pathOf = (f: UserNoteFolder): string => {
    const parts: string[] = [];
    const seen = new Set<string>();
    let cur: UserNoteFolder | undefined = f;
    while (cur && !seen.has(cur.id)) {
      seen.add(cur.id);
      parts.unshift(cur.name);
      cur = cur.parent_id ? byId.get(cur.parent_id) : undefined;
    }
    return parts.join(" / ");
  };
  return folders
    .filter((f) => !exclude.has(f.id))
    .map((f) => ({ value: f.id, label: pathOf(f) }))
    .sort((a, b) => a.label.localeCompare(b.label, undefined, { sensitivity: "base" }));
}

/**
 * Which row the open context menu is about (MAIN-634 AC-6).
 *
 * The shared menu (MAIN-167) reports neither what it opened over nor when it
 * closed, and this ticket may not change it — so the row marks itself on
 * `contextmenu` and the mark is dropped on every gesture that dismisses the
 * menu: a press outside it, a click on one of its items, Escape, a scroll. The
 * listeners exist only while a row is marked, which is a fraction of a second
 * at a time.
 */
function useContextTarget(): [string | null, (id: string) => void] {
  const [target, setTarget] = useState<string | null>(null);

  React.useEffect(() => {
    if (!target) return;
    const clear = () => setTarget(null);
    const onMouseDown = (e: MouseEvent) => {
      // A press INSIDE the menu is the menu being used, not dismissed; the
      // click that follows it clears the mark. Clearing here would drop the
      // highlight while the user is still choosing.
      const el = e.target instanceof Element ? e.target : null;
      if (!el?.closest(".ctxmenu")) clear();
    };
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") clear();
    };
    window.addEventListener("mousedown", onMouseDown, true);
    window.addEventListener("click", clear, true);
    window.addEventListener("keydown", onKeyDown, true);
    window.addEventListener("scroll", clear, true);
    return () => {
      window.removeEventListener("mousedown", onMouseDown, true);
      window.removeEventListener("click", clear, true);
      window.removeEventListener("keydown", onKeyDown, true);
      window.removeEventListener("scroll", clear, true);
    };
  }, [target]);

  return [target, setTarget];
}

export function Notebook() {
  const qc = useQueryClient();
  const [query, setQuery] = useState("");
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [error, setError] = useState<string | null>(null);
  const [ctxTarget, setCtxTarget] = useContextTarget();

  const { data: folders = [] } = useQuery({
    queryKey: ["notebook", "folders"],
    queryFn: async () => (await api.GET("/api/v1/notebook/folders")).data ?? [],
  });

  const { data: notes = [] } = useQuery({
    // The query is part of the key so a search is its own cache entry and the
    // server-side filter (AC-5) drives what the tree lists.
    queryKey: ["notebook", "notes", query],
    queryFn: async () =>
      (await api.GET("/api/v1/notebook/notes", { params: { query: { q: query || undefined } } }))
        .data ?? [],
  });

  const { data: openNote } = useQuery({
    enabled: !!selectedId,
    queryKey: ["notebook", "note", selectedId],
    queryFn: async () =>
      (
        await api.GET("/api/v1/notebook/notes/{id}", {
          params: { path: { id: selectedId! } },
        })
      ).data ?? null,
  });

  const tree = React.useMemo(() => buildTree(folders, notes), [folders, notes]);

  // A tree change (create/rename/delete/move) must show without a reload:
  // invalidate both flat lists and let react-query refetch + rebuild (AC-2).
  const refresh = () => {
    qc.invalidateQueries({ queryKey: ["notebook", "folders"] });
    qc.invalidateQueries({ queryKey: ["notebook", "notes"] });
  };

  const toggle = (id: string) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  // ── Writes ────────────────────────────────────────────────────────────────
  // Each clears the banner first, then surfaces the backend's message inline on
  // failure (AC-3) rather than letting it die in the console.

  const createNote = async (folderId: string | null) => {
    setError(null);
    const title = await askText({
      title: "New note",
      label: "Title",
      placeholder: "Untitled",
      confirmLabel: "create",
    });
    if (!title) return;
    const { data, error: err, response } = await api.POST("/api/v1/notebook/notes", {
      body: { title, folder_id: folderId },
    });
    if (err || !response.ok) return void setError(apiErrorMessage(err));
    refresh();
    if (data) setSelectedId(data.id);
  };

  const createFolder = async (parentId: string | null) => {
    setError(null);
    const name = await askText({ title: "New folder", label: "Name", confirmLabel: "create" });
    if (!name) return;
    const { error: err, response } = await api.POST("/api/v1/notebook/folders", {
      body: { name, parent_id: parentId },
    });
    if (err || !response.ok) return void setError(apiErrorMessage(err));
    if (parentId) setExpanded((p) => new Set(p).add(parentId));
    refresh();
  };

  const renameNote = async (note: UserNoteSummary) => {
    setError(null);
    const title = await askText({
      title: "Rename note",
      value: note.title,
      confirmLabel: "rename",
    });
    if (!title || title === note.title) return;
    const { error: err, response } = await api.PATCH("/api/v1/notebook/notes/{id}", {
      params: { path: { id: note.id } },
      body: buildNoteUpdate({ title }),
    });
    if (err || !response.ok) return void setError(apiErrorMessage(err));
    qc.invalidateQueries({ queryKey: ["notebook", "note", note.id] });
    refresh();
  };

  const renameFolder = async (folder: UserNoteFolder) => {
    setError(null);
    const name = await askText({
      title: "Rename folder",
      value: folder.name,
      confirmLabel: "rename",
    });
    if (!name || name === folder.name) return;
    const { error: err, response } = await api.PATCH("/api/v1/notebook/folders/{id}", {
      params: { path: { id: folder.id } },
      body: buildFolderUpdate({ name }),
    });
    if (err || !response.ok) return void setError(apiErrorMessage(err));
    refresh();
  };

  const moveNote = async (note: UserNoteSummary) => {
    setError(null);
    const choice = await askChoice({
      title: `Move "${note.title}"`,
      description: "Choose where this note should live.",
      choices: [
        { value: ROOT, label: "(root)", description: "no folder" },
        ...folderPathChoices(folders, new Set()),
      ],
      confirmLabel: "move",
    });
    if (!choice) return;
    // Tri-state (AC-4): ROOT → explicit null; a folder id → that id; the picker
    // can never produce "leave unchanged", so `move` is always present.
    const { error: err, response } = await api.PATCH("/api/v1/notebook/notes/{id}", {
      params: { path: { id: note.id } },
      body: buildNoteUpdate({ move: choice === ROOT ? null : choice }),
    });
    if (err || !response.ok) return void setError(apiErrorMessage(err));
    refresh();
  };

  const moveFolder = async (folder: UserNoteFolder) => {
    setError(null);
    // Self is excluded from the targets; a deeper-cycle attempt is caught by the
    // backend and surfaced inline.
    const choice = await askChoice({
      title: `Move folder "${folder.name}"`,
      description: "Choose the new parent folder.",
      choices: [
        { value: ROOT, label: "(root)", description: "top level" },
        ...folderPathChoices(folders, new Set([folder.id])),
      ],
      confirmLabel: "move",
    });
    if (!choice) return;
    const { error: err, response } = await api.PATCH("/api/v1/notebook/folders/{id}", {
      params: { path: { id: folder.id } },
      body: buildFolderUpdate({ move: choice === ROOT ? null : choice }),
    });
    if (err || !response.ok) return void setError(apiErrorMessage(err));
    refresh();
  };

  const deleteNote = async (note: UserNoteSummary) => {
    setError(null);
    const ok = await askConfirm({
      title: `Delete "${note.title}"?`,
      description: "This permanently removes the note.",
      confirmLabel: "delete",
      danger: true,
    });
    if (!ok) return;
    const { error: err, response } = await api.DELETE("/api/v1/notebook/notes/{id}", {
      params: { path: { id: note.id } },
    });
    if (err || !response.ok) return void setError(apiErrorMessage(err));
    if (selectedId === note.id) setSelectedId(null);
    refresh();
  };

  const deleteFolder = async (folder: UserNoteFolder) => {
    setError(null);
    const ok = await askConfirm({
      title: `Delete folder "${folder.name}"?`,
      description:
        "The folder is removed. Its notes and any sub-folders are kept and " +
        "reparented up one level (to the root if this was a top-level folder).",
      confirmLabel: "delete folder",
      danger: true,
    });
    if (!ok) return;
    const { error: err, response } = await api.DELETE("/api/v1/notebook/folders/{id}", {
      params: { path: { id: folder.id } },
    });
    if (err || !response.ok) return void setError(apiErrorMessage(err));
    refresh();
  };

  const saveContent = async (next: string) => {
    if (!openNote) return;
    setError(null);
    const { error: err, response } = await api.PATCH("/api/v1/notebook/notes/{id}", {
      params: { path: { id: openNote.id } },
      body: buildNoteUpdate({ content_md: next }),
    });
    if (err || !response.ok) {
      setError(apiErrorMessage(err));
      // Throw so EditableMarkdown keeps the draft in the editor instead of
      // dropping the edit as if it had saved.
      throw new Error("save failed");
    }
    qc.invalidateQueries({ queryKey: ["notebook", "note", openNote.id] });
  };

  const saveTitle = async (title: string): Promise<boolean> => {
    if (!openNote) return false;
    setError(null);
    const { error: err, response } = await api.PATCH("/api/v1/notebook/notes/{id}", {
      params: { path: { id: openNote.id } },
      body: buildNoteUpdate({ title }),
    });
    if (err || !response.ok) {
      setError(apiErrorMessage(err));
      return false;
    }
    qc.invalidateQueries({ queryKey: ["notebook", "note", openNote.id] });
    refresh();
    return true;
  };

  // ── Menus (MAIN-634) ──────────────────────────────────────────────────────
  // Every item is one of the write handlers above, unchanged — the right-click
  // replaced the hover buttons as the way IN, not what they did.

  const folderMenu = (folder: UserNoteFolder): ContextMenuItem[] => [
    { label: "New note", onSelect: () => void createNote(folder.id) },
    { label: "New sub-folder", onSelect: () => void createFolder(folder.id) },
    { separator: true },
    { label: "Rename", onSelect: () => void renameFolder(folder) },
    { label: "Move…", onSelect: () => void moveFolder(folder) },
    { separator: true },
    { label: "Delete folder", danger: true, onSelect: () => void deleteFolder(folder) },
  ];

  const noteMenu = (note: UserNoteSummary): ContextMenuItem[] => [
    { label: "Rename", onSelect: () => void renameNote(note) },
    { label: "Move…", onSelect: () => void moveNote(note) },
    { separator: true },
    { label: "Delete", danger: true, onSelect: () => void deleteNote(note) },
  ];

  // The tree's own background, so the empty space below the rows creates at the
  // root — the same two things the panel header's buttons do.
  const rootMenu = (): ContextMenuItem[] => [
    { label: "New note", onSelect: () => void createNote(null) },
    { label: "New folder", onSelect: () => void createFolder(null) },
  ];

  const menus: RowMenus = { folder: folderMenu, note: noteMenu };

  const searching = query.trim().length > 0;
  const emptyTree = tree.folders.length === 0 && tree.rootNotes.length === 0;
  // A search narrows the note list, but a matching note in a collapsed folder
  // would be invisible — so while searching, every folder is treated as open so
  // the results actually show (AC-5).
  const effectiveExpanded = searching ? new Set(folders.map((f) => f.id)) : expanded;

  return (
    <div className="nook-grid cards notebook" style={{ gridTemplateColumns: "300px 1fr" }}>
      <Panel
        title="Notebook"
        actions={
          <span className="notebook-tree-actions">
            <button className="btn small" title="new note at root" onClick={() => createNote(null)}>
              <FilePlus size={12} />
            </button>
            <button
              className="btn small"
              title="new folder at root"
              onClick={() => createFolder(null)}
            >
              <FolderPlus size={12} />
            </button>
          </span>
        }
      >
        <NotesMcpBanner />
        <div className="notebook-search">
          <SearchInput
            onSearch={setQuery}
            placeholder="Search notes…"
            ariaLabel="Search notes"
            iconTitle="Search notes"
          />
        </div>
        {/* The region IS the scroller, so the space below the last row is inside
            it and right-clicking there is still the tree (AC-3). A row's own
            region is nested deeper and therefore wins (AC-4). */}
        <ContextMenuRegion className="notebook-tree" items={rootMenu}>
          {emptyTree ? (
            <Empty>
              {searching
                ? "No notes match that search."
                : "No notes yet. Use the buttons above to add a note or folder."}
            </Empty>
          ) : (
            <>
              {tree.folders.map((node) => (
                <FolderBranch
                  key={node.folder.id}
                  node={node}
                  depth={0}
                  expanded={effectiveExpanded}
                  selectedId={selectedId}
                  onToggle={toggle}
                  onSelect={setSelectedId}
                  menus={menus}
                  ctxTarget={ctxTarget}
                  onCtxTarget={setCtxTarget}
                />
              ))}
              {tree.rootNotes.map((note) => (
                <NoteRow
                  key={note.id}
                  note={note}
                  depth={0}
                  selected={selectedId === note.id}
                  onSelect={setSelectedId}
                  menus={menus}
                  ctxTarget={ctxTarget}
                  onCtxTarget={setCtxTarget}
                />
              ))}
            </>
          )}
        </ContextMenuRegion>
      </Panel>

      <Panel
        title={openNote ? openNote.title || "Untitled" : "No note selected"}
      >
        {error && (
          <div className="notebook-error" role="alert">
            <AlertCircle size={13} />
            <span>{error}</span>
            <button className="notebook-error-dismiss" title="dismiss" onClick={() => setError(null)}>
              <X size={12} />
            </button>
          </div>
        )}
        {!openNote ? (
          <Empty>Select a note to open it, or create one from the tree.</Empty>
        ) : (
          <div className="notebook-editor">
            <NoteTitle key={openNote.id} value={openNote.title} onSave={saveTitle} />
            <EditableMarkdown
              key={openNote.id}
              value={openNote.content_md ?? ""}
              onSave={saveContent}
            />
          </div>
        )}
      </Panel>
    </div>
  );
}

/** The editable title, with the 200-char cap surfaced as a live counter and a
 *  blank-title guard mirroring the server's 400 (MAIN-84). */
function NoteTitle({
  value,
  onSave,
}: {
  value: string;
  onSave: (title: string) => Promise<boolean>;
}) {
  const [draft, setDraft] = useState(value);
  React.useEffect(() => setDraft(value), [value]);
  const dirty = draft !== value;
  const blank = draft.trim().length === 0;

  const commit = async () => {
    if (!dirty) return;
    if (blank) {
      setDraft(value); // reject a blank title locally, like the API does
      return;
    }
    const ok = await onSave(draft);
    if (!ok) setDraft(value);
  };

  return (
    <div className="notebook-title-row">
      <input
        className="input notebook-title"
        value={draft}
        maxLength={MAX_NAME_LEN}
        placeholder="Untitled"
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter") (e.target as HTMLInputElement).blur();
          if (e.key === "Escape") setDraft(value);
        }}
      />
      <span className={`notebook-title-count${draft.length >= MAX_NAME_LEN ? " at-limit" : ""}`}>
        {draft.length}/{MAX_NAME_LEN}
      </span>
      {dirty && !blank && (
        <button className="btn small primary" onClick={commit}>
          save title
        </button>
      )}
    </div>
  );
}

interface RowMenus {
  folder: (folder: UserNoteFolder) => ContextMenuItem[];
  note: (note: UserNoteSummary) => ContextMenuItem[];
}

/** What every row needs to own its right-click: the menu to offer, and the mark
 *  that says the open menu is about this row (AC-6). */
interface RowContext {
  menus: RowMenus;
  ctxTarget: string | null;
  onCtxTarget: (id: string) => void;
}

function FolderBranch({
  node,
  depth,
  expanded,
  selectedId,
  onToggle,
  onSelect,
  menus,
  ctxTarget,
  onCtxTarget,
}: {
  node: FolderNode;
  depth: number;
  expanded: Set<string>;
  selectedId: string | null;
  onToggle: (id: string) => void;
  onSelect: (id: string) => void;
} & RowContext) {
  const isOpen = expanded.has(node.folder.id);
  const rows: RowContext = { menus, ctxTarget, onCtxTarget };
  return (
    <div className="notebook-branch">
      <ContextMenuRegion
        style={{ display: "contents" }}
        items={() => menus.folder(node.folder)}
      >
        <div
          className={`notebook-row notebook-folder-row${
            ctxTarget === node.folder.id ? " ctx-target" : ""
          }`}
          style={{ paddingLeft: 6 + depth * 14 }}
          onClick={() => onToggle(node.folder.id)}
          onContextMenu={() => onCtxTarget(node.folder.id)}
          title={node.folder.name}
        >
          {isOpen ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
          {isOpen ? <FolderOpen size={13} /> : <Folder size={13} />}
          <span className="notebook-label">{node.folder.name}</span>
        </div>
      </ContextMenuRegion>
      {isOpen && (
        <>
          {node.children.map((child) => (
            <FolderBranch
              key={child.folder.id}
              node={child}
              depth={depth + 1}
              expanded={expanded}
              selectedId={selectedId}
              onToggle={onToggle}
              onSelect={onSelect}
              {...rows}
            />
          ))}
          {node.notes.map((note) => (
            <NoteRow
              key={note.id}
              note={note}
              depth={depth + 1}
              selected={selectedId === note.id}
              onSelect={onSelect}
              {...rows}
            />
          ))}
        </>
      )}
    </div>
  );
}

function NoteRow({
  note,
  depth,
  selected,
  onSelect,
  menus,
  ctxTarget,
  onCtxTarget,
}: {
  note: UserNoteSummary;
  depth: number;
  selected: boolean;
  onSelect: (id: string) => void;
} & RowContext) {
  return (
    <ContextMenuRegion style={{ display: "contents" }} items={() => menus.note(note)}>
      <div
        className={`notebook-row notebook-note-row${selected ? " selected" : ""}${
          ctxTarget === note.id ? " ctx-target" : ""
        }`}
        style={{ paddingLeft: 6 + depth * 14 + 13 }}
        onClick={() => onSelect(note.id)}
        onContextMenu={() => onCtxTarget(note.id)}
        title={note.title}
      >
        <FileText size={13} />
        <span className="notebook-label">{note.title || "Untitled"}</span>
      </div>
    </ContextMenuRegion>
  );
}
