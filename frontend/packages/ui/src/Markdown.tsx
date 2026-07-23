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
 * Edit markdown with a preview.
 *
 * Two panes rather than a WYSIWYG: the stored text IS the artifact — agents
 * parse `- [ ] **AC-1**` out of it — so an editor that rewrote the source into
 * its own idea of equivalent markdown would quietly break the contract the
 * whole loop depends on. You edit the real characters and see what they mean.
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
  const ref = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    if (autoFocus && tab === "write") ref.current?.focus();
  }, [autoFocus, tab]);

  /** Wrap the selection, or insert the markers and put the caret between. */
  const surround = (before: string, after = before) => {
    const el = ref.current;
    if (!el) return;
    const { selectionStart: s, selectionEnd: e } = el;
    const next = value.slice(0, s) + before + value.slice(s, e) + after + value.slice(e);
    onChange(next);
    // Restore the selection after React re-renders, or every shortcut would
    // dump the caret at the end of the document.
    requestAnimationFrame(() => {
      el.focus();
      el.setSelectionRange(s + before.length, e + before.length);
    });
  };

  /** Prefix every selected line — how lists and quotes actually get applied. */
  const prefixLines = (prefix: string) => {
    const el = ref.current;
    if (!el) return;
    const start = value.lastIndexOf("\n", el.selectionStart - 1) + 1;
    const end = value.indexOf("\n", el.selectionEnd);
    const stop = end === -1 ? value.length : end;
    const block = value
      .slice(start, stop)
      .split("\n")
      .map((l) => (l.startsWith(prefix) ? l.slice(prefix.length) : prefix + l))
      .join("\n");
    onChange(value.slice(0, start) + block + value.slice(stop));
    requestAnimationFrame(() => el.focus());
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    const mod = e.metaKey || e.ctrlKey;
    if (mod && e.key === "Enter") {
      e.preventDefault();
      onSave?.();
      return;
    }
    if (e.key === "Escape") {
      e.preventDefault();
      onCancel?.();
      return;
    }
    if (mod && e.key.toLowerCase() === "b") {
      e.preventDefault();
      surround("**");
    } else if (mod && e.key.toLowerCase() === "i") {
      e.preventDefault();
      surround("_");
    } else if (mod && e.key.toLowerCase() === "e") {
      e.preventDefault();
      surround("`");
    } else if (e.key === "Tab") {
      // Tab indents rather than leaving the field: this is a code-adjacent
      // editor and nested lists are common. Shift-Tab still escapes.
      if (!e.shiftKey) {
        e.preventDefault();
        prefixLines("  ");
      }
    }
  };

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

      {tab === "write" ? (
        <textarea
          ref={ref}
          className="md-source"
          style={{ minHeight }}
          value={value}
          placeholder={placeholder}
          onChange={(e) => onChange(e.target.value)}
          onKeyDown={onKeyDown}
          spellCheck
        />
      ) : (
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
