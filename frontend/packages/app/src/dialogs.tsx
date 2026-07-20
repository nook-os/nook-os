// In-app dialogs. A full-screen app shouldn't hand you a browser prompt, and
// window.prompt/confirm/alert can't be themed, can't hold more than one field,
// and block the whole tab. These are promise-based so call sites stay as
// straight-line as the originals:
//
//   const name = await askText({ title: "Rename session", value: current });
//   if (!name) return;
import React, { useEffect, useRef, useState } from "react";
import { create } from "zustand";

export interface DialogField {
  name: string;
  label?: string;
  value?: string;
  placeholder?: string;
  /** Render a textarea instead of a single-line input. */
  multiline?: boolean;
  /** Block submit while empty. */
  required?: boolean;
  /**
   * Mask the value. For passwords — the app password is typed in rooms, on
   * shared screens, and over screen shares, and it's the one thing that can't
   * be rotated if it's seen.
   */
  secret?: boolean;
  /** Autocomplete hint, e.g. "current-password" / "new-password". */
  autoComplete?: string;
}

export interface DialogChoice {
  value: string;
  label: string;
  description?: string;
}

interface DialogRequest {
  title: string;
  description?: string;
  /** A value the dialog exists to hand over — rendered with a copy button.
   *  For things shown exactly once, where "select the text" is a trap. */
  copy?: string;
  fields: DialogField[];
  choices?: DialogChoice[];
  confirmLabel: string;
  cancelLabel?: string;
  danger?: boolean;
  /** Values keyed by field name, plus `choice` when choices are shown. */
  resolve(value: Record<string, string> | null): void;
}

interface DialogState {
  current: DialogRequest | null;
  open(req: DialogRequest): void;
  close(value: Record<string, string> | null): void;
}

const useDialogStore = create<DialogState>((set, get) => ({
  current: null,
  open: (req) => {
    // Only one dialog at a time; a queued one would fight the modal layer.
    const existing = get().current;
    if (existing) existing.resolve(null);
    set({ current: req });
  },
  close: (value) => {
    const req = get().current;
    set({ current: null });
    req?.resolve(value);
  },
}));

function ask(
  req: Omit<DialogRequest, "resolve">,
): Promise<Record<string, string> | null> {
  return new Promise((resolve) => useDialogStore.getState().open({ ...req, resolve }));
}

/**
 * One-field text prompt. Resolves to the trimmed value, or null if cancelled.
 *
 * A secret is returned exactly as typed: trimming a password would quietly
 * disagree with the form that set it (which doesn't trim), making a password
 * with a leading or trailing space impossible to enter again.
 */
export async function askText(opts: {
  title: string;
  description?: string;
  label?: string;
  value?: string;
  placeholder?: string;
  multiline?: boolean;
  confirmLabel?: string;
  secret?: boolean;
  autoComplete?: string;
}): Promise<string | null> {
  const out = await ask({
    title: opts.title,
    description: opts.description,
    confirmLabel: opts.confirmLabel ?? "save",
    fields: [
      {
        name: "value",
        label: opts.label,
        value: opts.value,
        placeholder: opts.placeholder,
        multiline: opts.multiline,
        required: true,
        secret: opts.secret,
        autoComplete: opts.autoComplete,
      },
    ],
  });
  if (!out) return null;
  const value = out.value ?? "";
  return (opts.secret ? value : value.trim()) || null;
}

/** Multi-field form. Resolves to values keyed by field name. */
export async function askForm(opts: {
  title: string;
  description?: string;
  fields: DialogField[];
  confirmLabel?: string;
}): Promise<Record<string, string> | null> {
  return ask({
    title: opts.title,
    description: opts.description,
    fields: opts.fields,
    confirmLabel: opts.confirmLabel ?? "save",
  });
}

/** Yes/no. Resolves true only when confirmed. */
export async function askConfirm(opts: {
  title: string;
  description?: string;
  confirmLabel?: string;
  danger?: boolean;
}): Promise<boolean> {
  const out = await ask({
    title: opts.title,
    description: opts.description,
    fields: [],
    confirmLabel: opts.confirmLabel ?? "confirm",
    danger: opts.danger,
  });
  return out !== null;
}

/** Pick one of several options. Resolves to the chosen value, or null. */
export async function askChoice(opts: {
  title: string;
  description?: string;
  choices: DialogChoice[];
  confirmLabel?: string;
  danger?: boolean;
}): Promise<string | null> {
  const out = await ask({
    title: opts.title,
    description: opts.description,
    fields: [],
    choices: opts.choices,
    confirmLabel: opts.confirmLabel ?? "continue",
    danger: opts.danger,
  });
  return out ? (out.choice ?? null) : null;
}

/**
 * Message with a single dismiss — the themed replacement for alert().
 *
 * `copy` is for the case this app keeps running into: a secret shown once. It
 * gets its own copyable row, because the app disables text selection almost
 * everywhere and a token you can't select is a token you can't use.
 */
export async function notify(
  title: string,
  description?: string,
  opts: { copy?: string } = {},
): Promise<void> {
  await ask({
    title,
    description,
    copy: opts.copy,
    fields: [],
    confirmLabel: "ok",
    cancelLabel: "",
  });
}

/** A value plus a button that puts it on the clipboard. */
function CopyRow({ value }: { value: string }) {
  const [copied, setCopied] = useState(false);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(value);
    } catch {
      // Clipboard API needs a secure context and permission; when it is
      // refused, fall back to the old execCommand path rather than leaving
      // the button dead.
      const el = document.createElement("textarea");
      el.value = value;
      document.body.appendChild(el);
      el.select();
      try {
        document.execCommand("copy");
      } finally {
        document.body.removeChild(el);
      }
    }
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1500);
  };
  return (
    <div className="dialog-copy">
      <code>{value}</code>
      <button className="btn small" onClick={copy} title="copy to clipboard">
        {copied ? "copied" : "copy"}
      </button>
    </div>
  );
}

/** Renders the active dialog. Mounted once, next to the New Work host. */
export function DialogHost() {
  const current = useDialogStore((s) => s.current);
  const close = useDialogStore((s) => s.close);
  const [values, setValues] = useState<Record<string, string>>({});
  const [choice, setChoice] = useState<string>("");
  const firstRef = useRef<HTMLInputElement | HTMLTextAreaElement>(null);

  useEffect(() => {
    if (!current) return;
    setValues(
      Object.fromEntries(current.fields.map((f) => [f.name, f.value ?? ""])),
    );
    setChoice(current.choices?.[0]?.value ?? "");
    // Focus (and select) the first field so typing just works.
    const id = window.setTimeout(() => {
      firstRef.current?.focus();
      firstRef.current?.select?.();
    }, 30);
    return () => window.clearTimeout(id);
  }, [current]);

  if (!current) return null;

  const missing = current.fields.some(
    (f) => f.required && !(values[f.name] ?? "").trim(),
  );
  const submit = () => {
    if (missing) return;
    close({ ...values, ...(current.choices ? { choice } : {}) });
  };

  return (
    <div className="modal-backdrop" onMouseDown={() => close(null)}>
      <div
        className="modal dialog"
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key === "Escape") close(null);
          // Enter submits, except inside a textarea where it's a newline.
          if (e.key === "Enter" && !e.shiftKey) {
            const el = e.target as HTMLElement;
            if (el.tagName !== "TEXTAREA") {
              e.preventDefault();
              submit();
            }
          }
        }}
      >
        <div className="modal-header">{current.title}</div>
        <div className="modal-body">
          {current.description && (
            <p className="muted small dialog-description">{current.description}</p>
          )}

          {current.copy && <CopyRow value={current.copy} />}

          {current.fields.map((f, i) => (
            <div className="field" key={f.name}>
              {f.label && <label>{f.label}</label>}
              {f.multiline ? (
                <textarea
                  ref={i === 0 ? (firstRef as React.Ref<HTMLTextAreaElement>) : undefined}
                  className="input"
                  rows={4}
                  placeholder={f.placeholder}
                  value={values[f.name] ?? ""}
                  onChange={(e) =>
                    setValues((v) => ({ ...v, [f.name]: e.target.value }))
                  }
                />
              ) : (
                <input
                  ref={i === 0 ? (firstRef as React.Ref<HTMLInputElement>) : undefined}
                  className="input"
                  type={f.secret ? "password" : "text"}
                  autoComplete={f.autoComplete ?? (f.secret ? "off" : undefined)}
                  spellCheck={f.secret ? false : undefined}
                  autoCorrect={f.secret ? "off" : undefined}
                  autoCapitalize={f.secret ? "off" : undefined}
                  placeholder={f.placeholder}
                  value={values[f.name] ?? ""}
                  onChange={(e) =>
                    setValues((v) => ({ ...v, [f.name]: e.target.value }))
                  }
                />
              )}
            </div>
          ))}

          {current.choices?.map((c) => (
            <label
              key={c.value}
              className={`dialog-choice${choice === c.value ? " selected" : ""}`}
            >
              <input
                type="radio"
                name="dialog-choice"
                checked={choice === c.value}
                onChange={() => setChoice(c.value)}
              />
              <span>
                <span className="bright">{c.label}</span>
                {c.description && (
                  <span className="muted small dialog-choice-desc">
                    {c.description}
                  </span>
                )}
              </span>
            </label>
          ))}
        </div>
        <div className="modal-footer">
          <button
            className={`btn ${current.danger ? "danger" : "primary"}`}
            onClick={submit}
            disabled={missing}
          >
            {current.confirmLabel}
          </button>
          {current.cancelLabel !== "" && (
            <button className="btn" onClick={() => close(null)}>
              {current.cancelLabel ?? "cancel"}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
