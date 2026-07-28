// Card → markdown export (MAIN-188). These formats are the FEATURE — people
// paste the output into LLM chats, docs, and emails, so changing them later is a
// breaking change to their pipelines (that is why AC-6 snapshots them). Keep the
// functions pure and the formats exactly as documented here.

/** The task fields the exporter needs. Structurally satisfied by the board's
 *  `TaskItem`; kept minimal so the formatters are pure and easy to snapshot. */
export interface CardTask {
  key?: string | null;
  title: string;
  description?: string | null;
  type?: string | null;
  url?: string | null;
  labels?: readonly { name: string }[] | null;
}

/** A comment as the task-detail response carries it (MAIN-188 AC-3). */
export interface CardComment {
  author_name?: string | null;
  created_at: string;
  body_md: string;
}

/** Resolved display values the card row doesn't carry verbatim. */
export interface CardMeta {
  /** Priority label — "urgent" | "high" | "medium" | "low" | "none". */
  priorityLabel: string;
  /** The column's display name. */
  columnName: string;
}

/** *Body*: the description markdown verbatim (empty string when there is none). */
export function formatBody(task: CardTask): string {
  return task.description ?? "";
}

/** *Title + body*: `# KEY — Title`, a blank line, then the description. */
export function formatTitleBody(task: CardTask): string {
  return `# ${task.key ?? "—"} — ${task.title}\n\n${task.description ?? ""}`;
}

/**
 * *All*: title+body, then a metadata blockquote
 * `> type · priority · column · labels · url`, then `## Comments (N)` with each
 * comment as `**<author>** · <ISO timestamp>`, a blank line, then the comment
 * markdown — in chronological order. Zero comments → just the `## Comments (0)`
 * header with nothing under it.
 */
export function formatAll(
  task: CardTask,
  meta: CardMeta,
  comments: readonly CardComment[],
): string {
  const labels =
    (task.labels ?? []).map((l) => l.name).join(", ") || "no labels";
  const metaLine = `> ${task.type ?? "task"} · ${meta.priorityLabel} · ${meta.columnName} · ${labels} · ${task.url ?? "—"}`;

  // Chronological (oldest first); ISO timestamps sort lexicographically.
  const ordered = [...comments].sort((a, b) =>
    a.created_at < b.created_at ? -1 : a.created_at > b.created_at ? 1 : 0,
  );
  const header = `## Comments (${ordered.length})`;
  const body = ordered
    .map((c) => `**${c.author_name || "unknown"}** · ${c.created_at}\n\n${c.body_md}`)
    .join("\n\n");
  const commentsSection = ordered.length ? `${header}\n\n${body}` : header;

  return `${formatTitleBody(task)}\n\n${metaLine}\n\n${commentsSection}`;
}
