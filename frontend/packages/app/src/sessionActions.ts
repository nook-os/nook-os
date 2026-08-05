// Rename and Stop, as rules rather than as two copies of a dialog (MAIN-416).
//
// Both actions are offered from the PANE and from the session page, and neither
// is offered from the tab strip — that separation is the whole point of the
// epic, so it should not depend on two components happening to agree about what
// Stop says or when a name is legal.
//
// Kept pure so the parts that can actually be wrong are testable without a
// browser: what counts as an empty name, and what Stop tells you before you
// commit to it.

/** A name the server would accept, or the reason it would not. */
export type NameCheck =
  | { ok: true; name: string }
  | { ok: false; reason: string };

/**
 * Validate a session name the way `PATCH /api/v1/sessions/{id}` does — it
 * trims, then refuses empty (`routes/sessions.rs`).
 *
 * Checked HERE as well as there (AC-5) because a 400 arriving after the dialog
 * has closed is a worse way to learn you typed a space: the field is gone by
 * the time the answer comes back. Same rule, said earlier — not a different
 * rule, which would be its own bug.
 */
export function checkSessionName(input: string | null | undefined): NameCheck {
  if (input == null) return { ok: false, reason: "cancelled" };
  const name = input.trim();
  if (!name) return { ok: false, reason: "A session name cannot be empty." };
  return { ok: true, name };
}

/** Whether a rename is worth sending at all. */
export function renameIsANoop(current: string, next: string): boolean {
  return current.trim() === next.trim();
}

export interface StopPrompt {
  title: string;
  description: string;
  confirmLabel: string;
  danger: boolean;
}

/**
 * What Stop says before it happens.
 *
 * `danger: true` and a confirm, because ending a running process is not
 * undoable in the sense that matters — whatever was on screen is gone. But the
 * words are deliberately NOT the words of a kill: the row survives, the
 * declaration is still satisfied, and opening it starts it again (MAIN-415).
 * Somebody who reads this and expects to lose the session has been told the
 * wrong thing.
 */
export function stopPrompt(session: { name: string; managed?: boolean }): StopPrompt {
  return {
    title: `Stop ${session.name}?`,
    description: session.managed
      ? "The terminal ends and its ports are released. The session stays in " +
        "the workspace's declaration, so the reconciler will NOT start a " +
        "replacement — open it again when you want it back."
      : "The terminal ends and its ports are released. The session is kept, " +
        "so you can open it again later.",
    confirmLabel: "stop it",
    danger: true,
  };
}
