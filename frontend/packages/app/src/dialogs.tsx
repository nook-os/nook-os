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
}

export interface DialogChoice {
  value: string;
  label: string;
  description?: string;
}

interface DialogRequest {
  title: string;
  description?: string;
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

/** One-field text prompt. Resolves to the trimmed value, or null if cancelled. */
export async function askText(opts: {
  title: string;
  description?: string;
  label?: string;
  value?: string;
  placeholder?: string;
  multiline?: boolean;
  confirmLabel?: string;
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
      },
    ],
  });
  return out ? (out.value ?? "").trim() || null : null;
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

/** Message with a single dismiss — the themed replacement for alert(). */
export async function notify(title: string, description?: string): Promise<void> {
  await ask({ title, description, fields: [], confirmLabel: "ok", cancelLabel: "" });
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
