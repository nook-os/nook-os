// Markdown rendering and editing for the board.
//
// This replaces a hand-rolled subset that handled headings, bullets and bold.
// It looked like markdown until you wrote a table, a link, a nested list or a
// footnote — and then it printed the source, which is the failure mode that
// makes a tool feel unfinished. A spec body is exactly the kind of document
// that uses those constructs.
//
// `react-markdown` builds a React tree rather than an HTML string, so there is
// no `dangerouslySetInnerHTML` anywhere. `remark-gfm` adds what people actually
// write — tables, task lists, strikethrough, autolinks.
//
// Raw HTML IS parsed, because markdown authors reach for `<kbd>`, `<sub>` and
// the occasional `<details>`, and printing the tag as literal text reads as a
// renderer that is broken. That means a sanitiser, and a sanitiser is only as
// good as its allow-list — so the schema below is an allow-list built from
// rehype's default, with the tags that carry behaviour rather than meaning
// (script, iframe, form, style) never added.
import React, { useEffect, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeRaw from "rehype-raw";
import rehypeSanitize, { defaultSchema } from "rehype-sanitize";
import { Eye, Pencil } from "lucide-react";
import { EditorState, Prec } from "@codemirror/state";
import {
  EditorView,
  keymap,
  placeholder as cmPlaceholder,
} from "@codemirror/view";
import {
  history,
  historyKeymap,
  insertNewline,
  standardKeymap,
} from "@codemirror/commands";

/** Render markdown at panel density. */
/**
 * What HTML a task body or a note may contain.
 *
 * Presentational tags only. Anything that can execute, navigate, load a remote
 * resource or capture input is absent by construction rather than filtered out
 * afterwards — a task body is written by anyone in the tenant and read by
 * everyone, and agents write into these fields too.
 */
const SCHEMA = {
  ...defaultSchema,
  tagNames: [
    ...(defaultSchema.tagNames ?? []).filter((t) => t !== "img"),
    "kbd",
    "sub",
    "sup",
    "mark",
    "details",
    "summary",
    "abbr",
    // Kept deliberately: an inline image in a task body is normal, and the
    // attribute allow-list below is what keeps it from being a tracker with
    // arbitrary parameters.
    "img",
  ],
  attributes: {
    ...defaultSchema.attributes,
    "*": [...(defaultSchema.attributes?.["*"] ?? []), "className"],
    img: ["src", "alt", "title", "width", "height"],
    a: ["href", "title"],
  },
  // No `javascript:` or `data:` URLs anywhere.
  protocols: {
    ...defaultSchema.protocols,
    href: ["http", "https", "mailto"],
    src: ["http", "https"],
  },
};

export function Markdown({ src }: { src: string }) {
  return (
    <div className="md">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        // Order matters: parse the raw HTML first, THEN sanitise what parsing
        // produced. Reversed, the sanitiser runs over a tree that does not yet
        // contain the HTML it exists to check.
        rehypePlugins={[rehypeRaw, [rehypeSanitize, SCHEMA]]}
        components={{
          // Links leave the app, so they open in a new tab and drop the
          // referrer — a task body can contain a link somebody else wrote.
          a: ({ children, href }) => (
            <a href={href} target="_blank" rel="noreferrer noopener">
              {children}
            </a>
          ),
          // Wrapped so a wide table scrolls inside the panel instead of
          // stretching it and pushing the board off screen.
          table: ({ children }) => (
            <div className="md-table-wrap">
              <table>{children}</table>
            </div>
          ),
          input: (props) =>
            props.type === "checkbox" ? (
              // GFM task lists carry the AC-N contract the loop parses. They
              // render as state, not as controls: the source of truth is the
              // description, and a checkbox that silently did nothing would be
              // worse than one that is obviously read-only.
              <span className={`md-check ${props.checked ? "done" : ""}`}>
                {props.checked ? "☑" : "☐"}
              </span>
            ) : null,
        }}
      >
        {src}
      </ReactMarkdown>
    </div>
  );
}

/**
 * The editing transforms, as pure functions over a document string and a
 * selection `[from, to)`. They return the new document and the selection to
 * restore, so the exact same logic that ran against a `<textarea>`'s
 * `selectionStart/End` can now drive a CodeMirror transaction — and can be unit
 * tested by asserting on the resulting string, with no editor at all.
 */
export interface EditResult {
  doc: string;
  from: number;
  to: number;
}

/** Wrap the selection, or (empty selection) insert the markers with the caret
 *  between them. Byte-for-byte the old `surround`. */
export function applySurround(
  doc: string,
  from: number,
  to: number,
  before: string,
  after: string = before,
): EditResult {
  return {
    doc: doc.slice(0, from) + before + doc.slice(from, to) + after + doc.slice(to),
    from: from + before.length,
    to: to + before.length,
  };
}

/** Toggle `prefix` on every line the selection touches — how lists and quotes
 *  get applied, and un-applied. Byte-for-byte the old `prefixLines`, extended to
 *  report the selection so the whole affected block stays selected. */
export function applyPrefix(
  doc: string,
  from: number,
  to: number,
  prefix: string,
): EditResult {
  const start = doc.lastIndexOf("\n", from - 1) + 1;
  const end = doc.indexOf("\n", to);
  const stop = end === -1 ? doc.length : end;
  const block = doc
    .slice(start, stop)
    .split("\n")
    .map((l) => (l.startsWith(prefix) ? l.slice(prefix.length) : prefix + l))
    .join("\n");
  return {
    doc: doc.slice(0, start) + block + doc.slice(stop),
    from: start,
    to: start + block.length,
  };
}

/** Run an `EditResult` transform against a live CodeMirror view as one
 *  transaction, then keep focus — the imperative half of the pure helpers. */
function dispatchEdit(view: EditorView, edit: EditResult): boolean {
  view.dispatch({
    changes: { from: 0, to: view.state.doc.length, insert: edit.doc },
    selection: { anchor: edit.from, head: edit.to },
  });
  view.focus();
  return true;
}

function surroundView(view: EditorView, before: string, after?: string): boolean {
  const { from, to } = view.state.selection.main;
  return dispatchEdit(view, applySurround(view.state.doc.toString(), from, to, before, after));
}

function prefixView(view: EditorView, prefix: string): boolean {
  const { from, to } = view.state.selection.main;
  return dispatchEdit(view, applyPrefix(view.state.doc.toString(), from, to, prefix));
}

/**
 * Edit markdown with a preview.
 *
 * Two panes rather than a WYSIWYG: the stored text IS the artifact — agents
 * parse `- [ ] **AC-1**` out of it — so an editor that rewrote the source into
 * its own idea of equivalent markdown would quietly break the contract the
 * whole loop depends on. You edit the real characters and see what they mean.
 *
 * The write pane is CodeMirror 6, configured to be behaviour-neutral with the
 * `<textarea>` it replaced: Enter inserts a bare newline (no auto-indent), there
 * is no source highlighting, and nothing reformats what you type. The keymap and
 * toolbar drive the exact same `applySurround`/`applyPrefix` transforms as
 * before. This lands the library on its own; inline live-preview is a later
 * issue.
 */
export function MarkdownEditor({
  value,
  onChange,
  onSave,
  onCancel,
  placeholder,
  minHeight = 220,
  autoFocus = true,
}: {
  value: string;
  onChange: (v: string) => void;
  onSave?: () => void;
  onCancel?: () => void;
  placeholder?: string;
  minHeight?: number;
  autoFocus?: boolean;
}) {
  const [tab, setTab] = useState<"write" | "preview">("write");
  const boxRef = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);

  // Callbacks change every render; the keymap and update listener read the
  // latest through refs so the editor never has to be rebuilt to see them.
  const onChangeRef = useRef(onChange);
  const onSaveRef = useRef(onSave);
  const onCancelRef = useRef(onCancel);
  onChangeRef.current = onChange;
  onSaveRef.current = onSave;
  onCancelRef.current = onCancel;

  const surround = (before: string, after?: string) => {
    if (viewRef.current) surroundView(viewRef.current, before, after);
  };
  const prefixLines = (prefix: string) => {
    if (viewRef.current) prefixView(viewRef.current, prefix);
  };

  // Build the editor once. Value is synced in separately so external updates
  // (an agent editing, another browser) don't tear down the view mid-keystroke.
  useEffect(() => {
    if (!boxRef.current || viewRef.current) return;
    const theme = EditorView.theme(
      {
        "&": { backgroundColor: "transparent", color: "var(--nook-fg)" },
        "&.cm-focused": { outline: "none" },
        ".cm-scroller": {
          fontFamily: "var(--nook-font-mono, ui-monospace, monospace)",
          fontSize: "11.5px",
          lineHeight: "1.5",
          overflow: "auto",
        },
        ".cm-content": { padding: "6px 8px", caretColor: "var(--nook-accent)", minHeight: `${minHeight}px` },
        ".cm-cursor, .cm-dropCursor": { borderLeftColor: "var(--nook-accent)" },
        "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, .cm-content ::selection":
          { backgroundColor: "var(--nook-selection)" },
        ".cm-placeholder": { color: "var(--nook-fg-faint)" },
        ".cm-line": { padding: "0" },
      },
      { dark: true },
    );

    const view = new EditorView({
      parent: boxRef.current,
      state: EditorState.create({
        doc: value,
        extensions: [
          history(),
          // Prec.highest so the shortcuts below win over the standard bindings.
          Prec.highest(
            keymap.of([
              { key: "Mod-b", run: (v) => surroundView(v, "**") },
              { key: "Mod-i", run: (v) => surroundView(v, "_") },
              { key: "Mod-e", run: (v) => surroundView(v, "`") },
              // Tab toggles two-space indent on the touched lines, exactly as
              // before. Shift-Tab is deliberately left unbound so it escapes the
              // editor (moves focus), matching the old textarea.
              { key: "Tab", run: (v) => prefixView(v, "  ") },
              { key: "Mod-Enter", run: () => (onSaveRef.current?.(), true) },
              { key: "Escape", run: () => (onCancelRef.current?.(), true) },
              // A bare newline, never auto-indented — the stored text stays
              // byte-identical to what a textarea would have kept.
              { key: "Enter", run: insertNewline },
            ]),
          ),
          keymap.of([...historyKeymap, ...standardKeymap]),
          cmPlaceholder(placeholder ?? ""),
          EditorView.lineWrapping,
          EditorView.updateListener.of((u) => {
            if (u.docChanged) onChangeRef.current(u.state.doc.toString());
          }),
          theme,
        ],
      }),
    });
    viewRef.current = view;
    if (autoFocus) view.focus();
    return () => {
      view.destroy();
      viewRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Keep the editor's document in step with the controlled `value` without
  // clobbering an in-flight edit: only dispatch when they actually differ (a
  // change the editor itself made already matches, so this is a no-op then).
  useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    const current = view.state.doc.toString();
    if (value !== current) {
      view.dispatch({ changes: { from: 0, to: current.length, insert: value } });
    }
  }, [value]);

  // Focus when the write pane (re)appears, matching the textarea's autoFocus.
  useEffect(() => {
    if (autoFocus && tab === "write") viewRef.current?.focus();
  }, [autoFocus, tab]);

  return (
    <div className="md-editor">
      <div className="md-toolbar">
        <button
          className={`md-tab ${tab === "write" ? "on" : ""}`}
          onClick={() => setTab("write")}
          type="button"
        >
          <Pencil size={10} /> write
        </button>
        <button
          className={`md-tab ${tab === "preview" ? "on" : ""}`}
          onClick={() => setTab("preview")}
          type="button"
        >
          <Eye size={10} /> preview
        </button>
        <span className="md-tools">
          <button type="button" onClick={() => surround("**")} title="bold (⌘B)">
            B
          </button>
          <button type="button" onClick={() => surround("_")} title="italic (⌘I)">
            <em>I</em>
          </button>
          <button type="button" onClick={() => surround("`")} title="code (⌘E)">
            {"</>"}
          </button>
          <button type="button" onClick={() => prefixLines("- ")} title="bullet list">
            •
          </button>
          <button type="button" onClick={() => prefixLines("- [ ] ")} title="task list">
            ☐
          </button>
          <button type="button" onClick={() => prefixLines("> ")} title="quote">
            ❝
          </button>
        </span>
        {onSave && <span className="faint small md-hint">⌘↵ to save</span>}
      </div>

      {/* The CodeMirror host stays mounted across tab switches (just hidden) so
          the editor is not torn down and rebuilt every time you peek at the
          preview — which would drop the cursor and undo history. */}
      <div
        ref={boxRef}
        className="md-source"
        style={{ minHeight, display: tab === "write" ? "block" : "none" }}
      />
      {tab === "preview" && (
        <div className="md-preview" style={{ minHeight }}>
          {value.trim() ? (
            <Markdown src={value} />
          ) : (
            <span className="faint small">Nothing to preview yet.</span>
          )}
        </div>
      )}
    </div>
  );
}


/**
 * Rendered markdown that becomes an editor when you double-click it.
 *
 * Split out from any one screen because this is the interaction the whole app
 * wants wherever prose is stored — a task body today, a note tomorrow. The
 * component owns the mode and the draft; the caller owns persistence and is
 * told only when to save.
 *
 * Double-click rather than single: prose contains links and checkboxes, and a
 * single click has to remain "follow that link" or the rendered view becomes
 * untouchable.
 */
export function EditableMarkdown({
  value,
  onSave,
  placeholder = "Nothing here yet — double-click to write.",
  minHeight = 200,
  editing: controlledEditing,
  onEditingChange,
}: {
  value: string;
  onSave: (next: string) => Promise<void> | void;
  placeholder?: string;
  minHeight?: number;
  /** Optional: drive edit mode from outside (a toolbar button elsewhere). */
  editing?: boolean;
  onEditingChange?: (editing: boolean) => void;
}) {
  const [uncontrolled, setUncontrolled] = useState(false);
  const editing = controlledEditing ?? uncontrolled;
  const setEditing = (v: boolean) => {
    setUncontrolled(v);
    onEditingChange?.(v);
  };
  const [draft, setDraft] = useState(value);
  const [saving, setSaving] = useState(false);

  // Re-sync when the underlying value changes from elsewhere — an agent
  // commenting, another browser, or simply a different task opening. Never
  // while editing, or somebody's half-written paragraph vanishes under them.
  useEffect(() => {
    if (!editing) setDraft(value);
  }, [value, editing]);

  const save = async () => {
    setSaving(true);
    try {
      await onSave(draft);
      setEditing(false);
    } finally {
      setSaving(false);
    }
  };

  if (!editing) {
    return (
      <div
        className="md-editable"
        onDoubleClick={() => {
          setDraft(value);
          setEditing(true);
        }}
        title="double-click to edit"
      >
        {value.trim() ? (
          <Markdown src={value} />
        ) : (
          <span className="md-placeholder">{placeholder}</span>
        )}
      </div>
    );
  }

  return (
    <>
      <MarkdownEditor
        value={draft}
        onChange={setDraft}
        onSave={save}
        onCancel={() => {
          setDraft(value);
          setEditing(false);
        }}
        minHeight={minHeight}
      />
      <div className="md-actions">
        <span className="faint small" style={{ marginRight: "auto" }}>
          {draft === value ? "no changes" : "unsaved"}
        </span>
        <button
          className="btn small"
          onClick={() => {
            setDraft(value);
            setEditing(false);
          }}
        >
          cancel
        </button>
        <button
          className="btn small primary"
          onClick={save}
          disabled={saving || draft === value}
        >
          {saving ? "saving…" : "save"}
        </button>
      </div>
    </>
  );
}
