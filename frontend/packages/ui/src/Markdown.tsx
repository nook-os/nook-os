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
import remarkBreaks from "remark-breaks";
import remarkGfm from "remark-gfm";
import rehypeRaw from "rehype-raw";
import rehypeSanitize, { defaultSchema } from "rehype-sanitize";
import { Code2, Eye } from "lucide-react";
import { Compartment, EditorState, Prec } from "@codemirror/state";
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
import { livePreview } from "./markdownPreview";
import { MentionAnchor, MentionMenu, MentionOption } from "./MentionMenu";
import {
  applyMention,
  MentionLink,
  MentionTrigger,
  mentionTrigger,
  remarkMentions,
} from "./mentions";

/** Where the `@` picker's rows come from (MAIN-633). Injected rather than
 *  fetched here: this package renders markdown, and which endpoint answers
 *  "which workspaces can I mention" is the app's business. */
export interface MentionSource {
  search: (query: string) => Promise<MentionOption[]>;
}

/** Which editing surface the markdown editor shows. `live` renders markdown
 *  inline (Obsidian-style); `source` is the raw CodeMirror text. Persisted so a
 *  preference sticks across editor mounts (AC-4). */
export type MarkdownMode = "live" | "source";
const MODE_KEY = "nook.md-editor-mode";

export function loadMarkdownMode(): MarkdownMode {
  try {
    return localStorage.getItem(MODE_KEY) === "source" ? "source" : "live";
  } catch {
    return "live";
  }
}

function saveMarkdownMode(mode: MarkdownMode) {
  try {
    localStorage.setItem(MODE_KEY, mode);
  } catch {
    // storage unavailable — the preference just won't persist
  }
}

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

export function Markdown({
  src,
  onToggle,
  breaks = false,
  mentions,
}: {
  src: string;
  /** When present, rendered task-list checkboxes become clickable and call this
   *  with the checkbox's ordinal (0-based, source order) so the caller can flip
   *  the matching `- [ ] `/`- [x] ` in the source (MAIN-36). */
  onToggle?: (index: number) => void;
  /** Chat semantics: a single newline is a line break (remark-breaks).
   *
   *  Off for documents on purpose — a spec's soft-wrapped paragraph must stay
   *  one paragraph, which is what CommonMark says a single newline means. A
   *  chat message is the opposite case: the person pressed Shift+Enter to go
   *  down a line, and collapsing that into one run of prose renders their
   *  message wrong. */
  breaks?: boolean;
  /** The `@slug`s in `src` that RESOLVED to a workspace, and where each one
   *  goes (MAIN-633 AC-5). A slug absent from this list stays plain text — the
   *  reader must be able to tell a reference from a typo, and a link to nowhere
   *  says the opposite. */
  mentions?: MentionLink[];
}) {
  // Reset each render; react-markdown renders the inputs in document order, so
  // the Nth `input` is the Nth checkbox in source — the index `onToggle` gets.
  let checkboxIndex = 0;
  const remark = breaks ? [remarkGfm, remarkBreaks] : [remarkGfm];
  return (
    <div className="md">
      <ReactMarkdown
        remarkPlugins={
          mentions?.length ? [...remark, remarkMentions(mentions)] : remark
        }
        // Order matters: parse the raw HTML first, THEN sanitise what parsing
        // produced. Reversed, the sanitiser runs over a tree that does not yet
        // contain the HTML it exists to check.
        rehypePlugins={[rehypeRaw, [rehypeSanitize, SCHEMA]]}
        components={{
          // Links leave the app, so they open in a new tab and drop the
          // referrer — a task body can contain a link somebody else wrote.
          //
          // Unless they don't: a RELATIVE href is this app, and the mention
          // links MAIN-633 renders are all of that form. Sending `/workspaces/…`
          // to a second tab, with no referrer to drop and no third party to
          // hide it from, is the rule misapplied rather than obeyed.
          a: ({ children, href }) =>
            href && /^[a-z][a-z0-9+.-]*:/i.test(href) ? (
              <a href={href} target="_blank" rel="noreferrer noopener">
                {children}
              </a>
            ) : (
              <a href={href}>{children}</a>
            ),
          // Wrapped so a wide table scrolls inside the panel instead of
          // stretching it and pushing the board off screen.
          table: ({ children }) => (
            <div className="md-table-wrap">
              <table>{children}</table>
            </div>
          ),
          input: (props) => {
            if (props.type !== "checkbox") return null;
            const glyph = props.checked ? "☑" : "☐";
            // Without an onToggle the checkbox is read-only state (the source of
            // truth is the description text); a control that did nothing would
            // be worse than one that is obviously not a control.
            if (!onToggle) {
              return (
                <span className={`md-check ${props.checked ? "done" : ""}`}>
                  {glyph}
                </span>
              );
            }
            // Clickable: the Nth checkbox toggles the Nth source marker (AC-5/6).
            const i = checkboxIndex++;
            return (
              <button
                type="button"
                role="checkbox"
                aria-checked={!!props.checked}
                className={`md-check md-check-btn ${props.checked ? "done" : ""}`}
                title="toggle this item"
                onClick={(e) => {
                  e.preventDefault();
                  onToggle(i);
                }}
              >
                {glyph}
              </button>
            );
          },
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
  mentions,
}: {
  value: string;
  onChange: (v: string) => void;
  onSave?: () => void;
  onCancel?: () => void;
  placeholder?: string;
  minHeight?: number;
  autoFocus?: boolean;
  /** Typing `@` offers these (MAIN-633). Absent, `@` is an ordinary
   *  character — which is what every other editor in the app wants. */
  mentions?: MentionSource;
}) {
  const [mode, setMode] = useState<MarkdownMode>(loadMarkdownMode);
  const boxRef = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);
  // Holds the live-preview extension (or nothing, in source mode) so the two
  // modes are the SAME editor reconfigured — never a teardown, never a second
  // component. The document is untouched either way (AC-2).
  const previewRef = useRef(new Compartment());

  // Callbacks change every render; the keymap and update listener read the
  // latest through refs so the editor never has to be rebuilt to see them.
  const onChangeRef = useRef(onChange);
  const onSaveRef = useRef(onSave);
  const onCancelRef = useRef(onCancel);
  onChangeRef.current = onChange;
  onSaveRef.current = onSave;
  onCancelRef.current = onCancel;

  // ── the `@` picker (MAIN-633) ─────────────────────────────────────────────
  //
  // State is mirrored into refs because the keymap and the update listener are
  // built ONCE, with the editor, and would otherwise close over the first
  // render's values forever. Same reason as the callback refs above.
  const [menu, setMenu] = useState<{
    trigger: MentionTrigger;
    anchor: MentionAnchor;
  } | null>(null);
  const [options, setOptions] = useState<MentionOption[]>([]);
  const [loading, setLoading] = useState(false);
  const [active, setActive] = useState(0);

  const mentionsRef = useRef(mentions);
  const menuRef = useRef(menu);
  const optionsRef = useRef(options);
  const activeRef = useRef(active);
  mentionsRef.current = mentions;
  menuRef.current = menu;
  optionsRef.current = options;
  activeRef.current = active;
  /** The `@` offset Escape closed the menu over, so it stays closed while the
   *  caret is still in that token (AC-2) — a menu that sprang back on the next
   *  keystroke would make Escape look broken. */
  const dismissedRef = useRef<number | null>(null);
  /** Bumped per search, so a slow answer for `@n` cannot land after `@nook`. */
  const searchRef = useRef(0);

  const closeMenu = () => {
    searchRef.current++;
    menuRef.current = null;
    optionsRef.current = [];
    setMenu(null);
    setOptions([]);
    setLoading(false);
  };

  const runSearch = (source: MentionSource, query: string) => {
    const seq = ++searchRef.current;
    setLoading(true);
    Promise.resolve(source.search(query))
      .then((rows) => {
        if (seq !== searchRef.current) return;
        optionsRef.current = rows;
        activeRef.current = 0;
        setOptions(rows);
        setActive(0);
        setLoading(false);
      })
      .catch(() => {
        if (seq !== searchRef.current) return;
        // An empty menu says "nothing matches", which is the honest reading of
        // a failed lookup too: the caller cannot offer a completion either way.
        optionsRef.current = [];
        setOptions([]);
        setLoading(false);
      });
  };

  /** Where the menu hangs: under the `@` itself, so it reads as belonging to
   *  the word being typed. `coordsAtPos` needs layout, so under a test renderer
   *  it answers nothing and the editor's own box is the fallback. */
  const anchorFor = (view: EditorView, pos: number): MentionAnchor => {
    try {
      const at = view.coordsAtPos(pos);
      if (at) return { left: at.left, top: at.bottom + 4 };
    } catch {
      // no layout — fall through
    }
    const box = view.dom.getBoundingClientRect();
    return { left: box.left, top: box.bottom + 4 };
  };

  /** Recomputed on every doc or selection change: the caret's position IS the
   *  menu's open/closed state, so there is no flag to leave stale. */
  const syncMentions = (view: EditorView) => {
    const source = mentionsRef.current;
    if (!source) return;
    const sel = view.state.selection.main;
    const trigger = sel.empty
      ? mentionTrigger(view.state.doc.toString(), sel.head)
      : null;
    if (!trigger) {
      dismissedRef.current = null;
      if (menuRef.current) closeMenu();
      return;
    }
    if (dismissedRef.current === trigger.from) return;
    const open = menuRef.current;
    const next = { trigger, anchor: anchorFor(view, trigger.from) };
    menuRef.current = next;
    setMenu(next);
    if (open?.trigger.from === trigger.from && open.trigger.query === trigger.query) {
      return;
    }
    runSearch(source, trigger.query);
  };

  const insertMention = (view: EditorView, trigger: MentionTrigger, slug: string) => {
    const edit = applyMention(view.state.doc.toString(), trigger, slug);
    dismissedRef.current = null;
    closeMenu();
    // Dispatching also focuses, which is what puts the caret back in the prose
    // the person was writing (AC-3).
    dispatchEdit(view, edit);
  };

  // The three keys the menu owns. Each returns false when it is closed, so the
  // editor's own Enter/Escape bindings are untouched by this feature existing.
  const moveMention = (delta: number): boolean => {
    const count = optionsRef.current.length;
    if (!menuRef.current || !count) return false;
    const next = (activeRef.current + delta + count) % count;
    activeRef.current = next;
    setActive(next);
    return true;
  };

  const pickMention = (view: EditorView): boolean => {
    const open = menuRef.current;
    const option = optionsRef.current[activeRef.current];
    if (!open || !option) return false;
    insertMention(view, open.trigger, option.slug);
    return true;
  };

  const dismissMention = (): boolean => {
    const open = menuRef.current;
    if (!open) return false;
    dismissedRef.current = open.trigger.from;
    closeMenu();
    return true;
  };

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
              // First in the array, so the open menu answers arrows, Enter and
              // Escape before the editor does — and only while it is open.
              { key: "ArrowDown", run: () => moveMention(1) },
              { key: "ArrowUp", run: () => moveMention(-1) },
              { key: "Enter", run: pickMention },
              { key: "Escape", run: dismissMention },
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
          // Live-preview decorations, toggled via the compartment. The initial
          // contents match the persisted mode so the first paint is right.
          previewRef.current.of(loadMarkdownMode() === "live" ? livePreview() : []),
          EditorView.updateListener.of((u) => {
            if (u.docChanged) onChangeRef.current(u.state.doc.toString());
            if (u.docChanged || u.selectionSet) syncMentions(u.view);
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

  // Switch the live-preview decorations on or off by reconfiguring the
  // compartment — same editor, same document, only the display changes. Persist
  // the choice so it survives the next mount (AC-4).
  const switchMode = (next: MarkdownMode) => {
    setMode(next);
    saveMarkdownMode(next);
    const view = viewRef.current;
    if (!view) return;
    view.dispatch({
      effects: previewRef.current.reconfigure(next === "live" ? livePreview() : []),
    });
    if (autoFocus) view.focus();
  };

  return (
    <div className="md-editor">
      <div className="md-toolbar">
        <button
          className={`md-tab ${mode === "live" ? "on" : ""}`}
          onClick={() => switchMode("live")}
          type="button"
          title="rendered markdown, edited inline"
        >
          <Eye size={10} /> live
        </button>
        <button
          className={`md-tab ${mode === "source" ? "on" : ""}`}
          onClick={() => switchMode("source")}
          type="button"
          title="raw markdown source"
        >
          <Code2 size={10} /> source
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

      {/* One CodeMirror surface for both modes — live-preview is a decoration
          layer over it, so the caret moves through the rendered text and the
          stored document is never re-serialized. */}
      <div
        ref={boxRef}
        className={`md-source md-mode-${mode}`}
        style={{ minHeight }}
      />

      {mentions && menu && (
        <MentionMenu
          options={options}
          loading={loading}
          query={menu.trigger.query}
          active={active}
          anchor={menu.anchor}
          onPick={(option) => {
            const view = viewRef.current;
            if (view) insertMention(view, menu.trigger, option.slug);
          }}
        />
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
  onToggle,
  mentions,
  mentionLinks,
}: {
  value: string;
  onSave: (next: string) => Promise<void> | void;
  placeholder?: string;
  minHeight?: number;
  /** Optional: drive edit mode from outside (a toolbar button elsewhere). */
  editing?: boolean;
  onEditingChange?: (editing: boolean) => void;
  /** Optional: make rendered checkboxes clickable in the display view (MAIN-36). */
  onToggle?: (index: number) => void;
  /** Optional: complete `@` while editing (MAIN-633 AC-1). */
  mentions?: MentionSource;
  /** Optional: link the `@slug`s that resolved, while displaying (AC-5). */
  mentionLinks?: MentionLink[];
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
          <Markdown src={value} onToggle={onToggle} mentions={mentionLinks} />
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
        mentions={mentions}
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
