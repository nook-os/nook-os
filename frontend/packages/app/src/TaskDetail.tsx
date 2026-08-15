// The task modal: one whole issue, opened over the board.
//
// This was a split pane. A modal wins for the reason Jira and Linear both use
// one: a task body is a spec, and reading it in a 420px column beside four
// other columns meant every line wrapped twice. The board is one keypress away
// and the work here is reading and writing prose, not comparing cards.
import React, { useCallback, useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import {
  GitBranch,
  SquareTerminal,
  X,
  Ban,
  Link2,
  MoreHorizontal,
  ChevronDown,
  Plus,
} from "lucide-react";
import { api, type TaskAttachment, type TaskLabel, type RelatedTask } from "@nookos/api";
import {
  Pill,
  Markdown,
  MarkdownEditor,
  EditableMarkdown,
  Select,
  TypeSelect,
  VISIBILITY_META,
  useAnchoredMenu,
} from "@nookos/ui";
import { PRIORITIES } from "./taskmeta";
import {
  AttachButton,
  AttachmentList,
  DropZone,
  UploadTray,
  pastedFiles,
  useUploads,
} from "./TaskAttachments";
import { TaskPicker, isDone, type PickerTask } from "./TaskPicker";
import { WorkspacePicker } from "./WorkspacePicker";
import { toggleTaskCheckbox } from "./taskCheckbox";
import { TaskInteractions } from "./Interactions";
import { LoopPanel } from "./LoopPanel";

/** The "no workspace" option's value. `Select` needs a string, and an empty
 *  one cannot collide with a uuid. */
const NO_WORKSPACE = "";
/** The "no epic" option's value (MAIN-83) — same empty-string convention. */
const NO_EPIC = "";
/** The three labels that STOP a card, mirroring the server's `ESCALATION_LABELS`
 *  — raised by different machinery (an agent's `blocked` outcome, the starved
 *  queue reaper, the failure ladder) but saying one thing between them: a person
 *  must look at this. A card carrying any of them is one "Comment and unblock"
 *  can restart. */
const ESCALATION_LABELS = ["blocked", "spec-blocked", "needs-human-review"];

/** Whether "Comment and unblock" is offered at all (MAIN-584 AC-12). A LABEL
 *  stop, which is a different mechanism from the `blocks` relation graph the
 *  banner reads — that one is untouched (NG-9). */
export function isEscalated(labels: TaskLabel[] | undefined): boolean {
  return (labels ?? []).some((l) => ESCALATION_LABELS.includes(l.name));
}

/** The request the composer sends. Without the unblock button it must carry no
 *  `clear_escalation` AT ALL, not `false`: an ordinary comment reaches the
 *  server exactly as it always did, so a question can still be asked on a
 *  stopped card (MAIN-584 NG-4). `request_changes` (MAIN-591) is the same
 *  shape, and the two are independent. */
export function commentRequestBody(
  body_md: string,
  unblock?: boolean,
  requestChanges?: boolean,
) {
  return {
    body_md,
    ...(unblock ? { clear_escalation: true } : {}),
    ...(requestChanges ? { request_changes: true } : {}),
  };
}

/** Whether "request changes" is offered at all (MAIN-591 AC-10): the card
 *  records a pull request, and that pull request is still open.
 *
 *  Openness is read from the CARD'S COLUMN rather than from GitHub, because the
 *  task detail carries no PR state and asking the forge from here would be a
 *  request per render. `merge_reconcile` moves a card to a done column when its
 *  PR merges or closes, so the two agree in every case the loop produces — and
 *  the server reads the pull request itself before writing anything (AC-2), so
 *  a board that has drifted only ever offers a button that then refuses. */
export function canRequestChanges(
  prUrl: string | null | undefined,
  columnType: string | undefined,
): boolean {
  return !!prUrl && columnType !== "completed" && columnType !== "canceled";
}

export function TaskDetail({
  taskId,
  columns,
  epics,
  onClose,
  onMenu,
}: {
  taskId: string;
  /** The board's columns, so state can be changed from here. */
  columns: { id: string; name: string; type?: string }[];
  /** The board's epics, for the Epic picker (MAIN-83 AC-4). Omitted → no picker. */
  epics?: { id: string; key?: string | null; title: string }[];
  onClose: () => void;
  /** Open the same action menu the cards use, anchored to the ⋯ button. */
  onMenu?: (anchor: { x: number; y: number }) => void;
}) {
  const qc = useQueryClient();
  const [body, setBody] = useState("");
  const [editing, setEditing] = useState(false);

  // Switching tasks must not carry a half-written description across to a
  // different issue — the panel stays mounted, only its id changes.
  useEffect(() => {
    setEditing(false);
    setBody("");
    setPending([]);
  }, [taskId]);

  const { data, isLoading } = useQuery({
    queryKey: ["task", taskId],
    queryFn: async () =>
      (await api.GET("/api/v1/tasks/{id}", { params: { path: { id: taskId } } }))
        .data,
  });
  const { data: allLabels } = useQuery({
    queryKey: ["labels"],
    queryFn: async () => (await api.GET("/api/v1/labels")).data ?? [],
  });

  const bust = () => {
    qc.invalidateQueries({ queryKey: ["task", taskId] });
    qc.invalidateQueries({ queryKey: ["boards"] });
  };

  // ── Attachments (MAIN-533) ──────────────────────────────────────────────
  //
  // ONE request for the whole thread — the ticket's files and every comment's —
  // because a page with N comments would otherwise make N+1 of them. Grouped
  // below by the `parent_kind`/`parent_id` each record already carries.
  const { data: attachments } = useQuery({
    queryKey: ["task-attachments", taskId],
    queryFn: async () =>
      (
        await api.GET("/api/v1/tasks/{id}/attachments", {
          params: { path: { id: taskId }, query: { include: "comments" } },
        })
      ).data ?? [],
  });
  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
  const bustAttachments = () =>
    qc.invalidateQueries({ queryKey: ["task-attachments", taskId] });

  const attachToTask = async (contentId: string) => {
    await api.POST("/api/v1/tasks/{id}/attachments", {
      params: { path: { id: taskId } },
      body: { user_content_id: contentId },
    });
    await bustAttachments();
    // The board card's count comes off the board read, not this one.
    qc.invalidateQueries({ queryKey: ["boards"] });
  };
  const ticketUploads = useUploads(attachToTask);

  // A file pasted or dropped into the composer belongs to the comment being
  // written — which has no id yet. So the bytes go up now and the join waits
  // here until the comment exists.
  const [pending, setPending] = useState<{ contentId: string; name: string }[]>([]);
  const commentUploads = useUploads((contentId, file) =>
    setPending((prev) => [...prev, { contentId, name: file.name }]),
  );

  const removeAttachment = async (id: string) => {
    await api.DELETE("/api/v1/attachments/{id}", { params: { path: { id } } });
    await bustAttachments();
    qc.invalidateQueries({ queryKey: ["boards"] });
  };

  /// Offered to the person who attached it and to a tenant owner/admin — the
  /// same rule the server enforces (AC-6).
  const canRemove = (a: TaskAttachment) =>
    a.attached_by === me?.user.id ||
    me?.user.role === "owner" ||
    me?.user.role === "admin";

  const forParent = (kind: string, id: string) =>
    (attachments ?? []).filter((a) => a.parent_kind === kind && a.parent_id === id);

  const comment = useMutation({
    mutationFn: async ({
      body_md,
      unblock,
      requestChanges,
    }: {
      body_md: string;
      unblock?: boolean;
      requestChanges?: boolean;
    }) => {
      const created = (
        await api.POST("/api/v1/tasks/{id}/comments", {
          params: { path: { id: taskId } },
          body: commentRequestBody(body_md, unblock, requestChanges),
        })
      ).data;
      // The comment exists now, so its waiting files can be hung on it. A
      // failure here is reported by the shared write-failure path and leaves
      // the comment posted rather than the text lost.
      if (created) {
        for (const p of pending) {
          await api.POST("/api/v1/comments/{id}/attachments", {
            params: { path: { id: created.id } },
            body: { user_content_id: p.contentId },
          });
        }
      }
      return created;
    },
    // Clear the composer only when a comment actually exists. `api.POST` does
    // not throw on a 4xx — the refusal goes to the shared write-failure path
    // and this still resolves — so an unconditional reset threw away the text
    // the server had just refused. Request-changes is what made that routine:
    // the button is offered from the card's column, and the server checks the
    // pull request (MAIN-591 AC-2), so a refusal is an expected answer rather
    // than a corner.
    onSuccess: (created) => {
      if (created) {
        setBody("");
        setPending([]);
      }
      void bustAttachments();
      bust();
    },
  });

  // ── Dependencies (MAIN-194) ─────────────────────────────────────────────
  //
  // ONE server contract, `BLOCKER blocks DEPENDENT`, reached from two entry
  // points. The direction is resolved here, once, so no caller ever reasons
  // about argument order — a reversed relation silently inverts the loop's
  // build order, which is the failure this mapping exists to make impossible.
  //
  //   "Blocked by X"  → X is the blocker  → POST /tasks/X/relations {to: this}
  //   "Blocks X"      → this is the blocker → POST /tasks/this/relations {to: X}
  const [addingDep, setAddingDep] = useState<null | "blocked_by" | "blocks">(null);

  const linkBlocking = async (
    direction: "blocked_by" | "blocks",
    other: PickerTask,
    // The RESOLVED uuid, never the `taskId` prop: the board opens this modal by
    // KEY (`?task=NOOK-1`), the path param accepts either, but `to_task` is a
    // uuid and a key there is rejected. Same handoff MAIN-209 pinned for the
    // loop panel — and it failed silently here until a live click proved it.
    thisTaskId: string,
  ) => {
    const [blocker, dependent] =
      direction === "blocked_by" ? [other.id, thisTaskId] : [thisTaskId, other.id];
    await api.POST("/api/v1/tasks/{id}/relations", {
      params: { path: { id: blocker } },
      body: { to_task: dependent, kind: "blocks" },
    });
    setAddingDep(null);
    bust();
  };

  const unlink = async (relationId: string) => {
    await api.DELETE("/api/v1/relations/{id}", {
      params: { path: { id: relationId } },
    });
    bust();
  };

  const toggleLabel = async (label: TaskLabel, on: boolean) => {
    const path = { params: { path: { id: taskId, label: label.name } } };
    if (on) await api.PUT("/api/v1/tasks/{id}/labels/{label}", path);
    else await api.DELETE("/api/v1/tasks/{id}/labels/{label}", path);
    bust();
  };

  // Create a brand-new label and put it on this task in one gesture. The old
  // picker could only attach labels that already existed, so a tenant with no
  // labels — or one that needed a new one like `agent-ready` — had no way to
  // make one from the UI at all. POST is idempotent server-side (upsert on
  // name), so racing two identical creates is safe.
  const createLabel = async (name: string) => {
    const label = name.trim();
    if (!label) return;
    await api.POST("/api/v1/labels", { body: { name: label } });
    await api.PUT("/api/v1/tasks/{id}/labels/{label}", {
      params: { path: { id: taskId, label } },
    });
    qc.invalidateQueries({ queryKey: ["labels"] });
    bust();
  };

  const saveDescription = async (description: string) => {
    // Guard the save on the version we opened the editor at (AC-7): if the body
    // changed under us the server returns 409 and applies nothing — the global
    // write-failure toast carries the message, and we reload so the view shows
    // the current content rather than the edit that did not apply.
    const { error, response } = await api.PATCH("/api/v1/tasks/{id}", {
      params: { path: { id: taskId } },
      body: { description, expected_updated_at: data?.task.updated_at },
    });
    if (!error && response.ok) setEditing(false);
    bust();
  };

  // Clicking a rendered checkbox flips the matching source marker through the
  // safe path: read the current body+version, toggle only that occurrence, PATCH
  // with the guard. On a concurrent edit (409) re-read the fresh body, re-apply
  // the toggle, and retry once; a second conflict reloads rather than clobbers
  // (AC-5/AC-6). It rides the same TaskChanged as any edit, so other viewers and
  // the board update live (AC-8).
  const toggleCheckbox = async (index: number) => {
    let base = data?.task;
    if (!base) return;
    for (let attempt = 0; attempt < 2; attempt++) {
      const next = toggleTaskCheckbox(base.description ?? "", index);
      const { error, response } = await api.PATCH("/api/v1/tasks/{id}", {
        params: { path: { id: taskId } },
        body: { description: next, expected_updated_at: base.updated_at },
      });
      if (!error && response.ok) {
        bust();
        return;
      }
      if (response.status === 409 && attempt === 0) {
        const fresh = (
          await api.GET("/api/v1/tasks/{id}", { params: { path: { id: taskId } } })
        ).data;
        if (fresh?.task) {
          base = fresh.task;
          continue; // re-apply the toggle to the fresh body and retry once
        }
      }
      bust(); // a second conflict or other error — reload; the toast explains
      return;
    }
  };

  const saveTitle = async (title: string) => {
    await api.PATCH("/api/v1/tasks/{id}", {
      params: { path: { id: taskId } },
      body: { title },
    });
    bust();
  };

  const moveTo = async (column_id: string) => {
    await api.PATCH("/api/v1/tasks/{id}", {
      params: { path: { id: taskId } },
      body: { column_id },
    });
    bust();
  };

  // Claim and release are one control, because they are one question: is this
  // mine? Two buttons would leave both on screen with only one ever valid.
  const toggleClaim = async (claimed: boolean) => {
    const path = { params: { path: { id: taskId } } };
    if (claimed) await api.POST("/api/v1/tasks/{id}/release", path);
    else await api.POST("/api/v1/tasks/{id}/claim", { ...path, body: {} });
    bust();
  };

  const setPriority = async (priority: number) => {
    await api.PATCH("/api/v1/tasks/{id}", {
      params: { path: { id: taskId } },
      body: { priority },
    });
    bust();
  };

  const setType = async (type: string) => {
    await api.PATCH("/api/v1/tasks/{id}", {
      params: { path: { id: taskId } },
      body: { type },
    });
    bust();
  };

  /** Change who may see this card (MAIN-103). The MAIN-85 gate answers 403 "this
   *  needs tenant owner or admin" when the caller may not — surfaced INLINE on
   *  the selector, not as a toast or console noise. A 404 means the card became
   *  invisible under us, so close the modal the way a vanished card is handled.
   *  Returns the outcome so the selector can render the inline error. */
  const setVisibility = async (
    visibility: string,
  ): Promise<{ ok: boolean; status: number }> => {
    const { error, response } = await api.PATCH("/api/v1/tasks/{id}", {
      params: { path: { id: taskId } },
      body: { visibility },
    });
    if (!error && response.ok) {
      bust();
      return { ok: true, status: response.status };
    }
    if (response.status === 404) {
      onClose();
      return { ok: false, status: 404 };
    }
    // 403 (and anything else): leave the card as it was and let the selector say so.
    return { ok: false, status: response.status };
  };

  /** Which repo this ticket is work on. `""` means none, sent as null. */
  const setWorkspace = async (id: string) => {
    await api.PATCH("/api/v1/tasks/{id}", {
      params: { path: { id: taskId } },
      // Null, not omitted: the field is absent-or-null-or-value on the wire,
      // and omitting it is how you say "leave this alone".
      body: { workspace_id: id === NO_WORKSPACE ? null : id },
    });
    bust();
    // A task's workspace decides which board a confined agent sees it on, so
    // the lists that filter by workspace are now wrong until they refetch.
    qc.invalidateQueries({ queryKey: ["tasks"] });
  };

  /** File this task under an epic (MAIN-83 AC-4). Tri-state like workspace: `""`
   *  → null = detach; a uuid = move under that epic. A backend rejection (a
   *  non-epic parent) surfaces via the write-failure toast, untouched (AC-6). */
  const setParent = async (id: string) => {
    await api.PATCH("/api/v1/tasks/{id}", {
      params: { path: { id: taskId } },
      body: { parent: id === NO_EPIC ? null : id },
    });
    bust();
  };

  if (isLoading || !data) {
    return (
      <Shell onClose={onClose}>
        <div className="faint small" style={{ padding: 16 }}>
          Loading…
        </div>
      </Shell>
    );
  }

  const { task, comments, blocked_by, blocking, related, is_blocked, children } = data;
  const taskAttachments = forParent("task", task.id);
  const linked = [...blocked_by, ...blocking, ...related];
  const isEpic = task.type === "epic";
  const escalated = isEscalated(task.labels);
  const canReject = canRequestChanges(
    task.pr_url,
    columns.find((c) => c.id === task.column_id)?.type,
  );

  return (
    <Shell onClose={onClose}>
      <div className="modal-header task-modal-head">
        <span className="mono bright">{task.key ?? "task"}</span>
        <span className="task-modal-head-actions">
          {onMenu && (
            <button
              className="btn small"
              title="actions"
              onClick={(e) => {
                const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
                onMenu({ x: r.right - 180, y: r.bottom + 4 });
              }}
            >
              <MoreHorizontal size={13} />
            </button>
          )}
          <button className="btn small" onClick={onClose} title="close (Esc)">
            <X size={12} />
          </button>
        </span>
      </div>

      {/* Two panes: the WORK on the left, the FACTS about it on the right.
          The split is what makes a long spec readable — prose gets the width,
          and the fields that are read at a glance stop interrupting it. */}
      <div className="task-panes">
        <DropZone className="task-main" onFiles={ticketUploads.add}>
          {/* Upper-left, by the title: the type is what a ticket IS, so it
              classifies the spec before you read it (AC-1). */}
          <TypeSelect value={task.type} onChange={setType} />
          {/* Editable in place: renaming is the most common edit there is. */}
          <input
            className="task-modal-title"
            defaultValue={task.title}
            key={task.id}
            onBlur={(e) => {
              const v = e.target.value.trim();
              if (v && v !== task.title) void saveTitle(v);
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter") (e.target as HTMLInputElement).blur();
              if (e.key === "Escape") {
                (e.target as HTMLInputElement).value = task.title;
                (e.target as HTMLInputElement).blur();
              }
            }}
          />

          {is_blocked && (
            <div className="task-blocked-banner">
              <Ban size={12} /> Blocked by{" "}
              {blocked_by
                .filter(
                  (r) => r.column_type !== "completed" && r.column_type !== "canceled",
                )
                .map((r) => r.key ?? r.title)
                .join(", ")}
            </div>
          )}

          <LabelField
            all={allLabels ?? []}
            on={task.labels ?? []}
            onToggle={toggleLabel}
            onCreate={createLabel}
          />

          {/* An agent blocked on a question is the most time-sensitive thing on
              a ticket, so its ask sits above the spec, not buried below it. Only
              renders when there is at least one pending interaction (MAIN-159). */}
          <TaskInteractions taskId={taskId} />

          {/* The ticket's own loop run (MAIN-128): the spec/decompose job and
              its live transcript. Pass the resolved UUID (`task.id`), not the
              prop `taskId` — the board opens the modal by KEY, and the jobs API
              is UUID-keyed, so forwarding the key 400s the list and 422s create
              (MAIN-209). */}
          <LoopPanel taskId={task.id} taskType={task.type} taskKey={task.key} />

          <div className="task-section">
            <div className="task-section-h">
              description
              {!editing && (
                <span className="faint md-hint-inline">double-click to edit</span>
              )}
            </div>
            <EditableMarkdown
              value={task.description ?? ""}
              editing={editing}
              onEditingChange={setEditing}
              onSave={saveDescription}
              onToggle={toggleCheckbox}
              placeholder="No description yet — double-click to write the acceptance criteria."
            />
          </div>

          {/* The ticket's own files (MAIN-533). Dropping anywhere on the left
              pane attaches here — a spec or a screenshot is context for the
              whole issue, and making people find a small target for that is
              how attachments end up pasted into comments instead. */}
          <div className="task-section">
            <div className="task-section-h">
              attachments · {taskAttachments.length}
              <span style={{ marginLeft: "auto" }}>
                <AttachButton onFiles={ticketUploads.add} />
              </span>
            </div>
            {taskAttachments.length === 0 && ticketUploads.uploads.length === 0 && (
              <div className="faint small">
                Drop a file here, paste one into a comment, or use Attach.
              </div>
            )}
            <AttachmentList
              attachments={taskAttachments}
              canRemove={canRemove}
              onRemove={(id) => void removeAttachment(id)}
            />
            <UploadTray
              uploads={ticketUploads.uploads}
              onRetry={ticketUploads.retry}
              onDismiss={ticketUploads.dismiss}
            />
          </div>

          {/* Dependencies (MAIN-194): the two BLOCKING directions, editable.
              `related`/`duplicates` stay read-only below — a later card adds
              kinds to this same section (NG-1). They are listed there and not
              here so no relation appears twice. */}
          <div className="task-section">
            <div className="task-section-h">dependencies</div>
            {(
              [
                ["blocked by", "blocked_by", blocked_by],
                ["blocks", "blocks", blocking],
              ] as [string, "blocked_by" | "blocks", RelatedTask[]][]
            ).map(([label, direction, list]) => (
              <div key={direction} className="task-dep-group">
                <div className="task-dep-head">
                  <span className="faint small">{label}</span>
                  <button
                    type="button"
                    className="btn small"
                    title={
                      direction === "blocked_by"
                        ? "add a ticket that blocks this one"
                        : "add a ticket this one blocks"
                    }
                    onClick={() => setAddingDep(addingDep === direction ? null : direction)}
                  >
                    <Plus size={11} /> Add
                  </button>
                </div>
                {list.length === 0 && addingDep !== direction && (
                  <span className="faint small">none</span>
                )}
                {list.map((r) => (
                  <div key={r.relation_id} className="task-dep-row">
                    <Link2 size={10} />
                    <span className="mono">{r.key ?? "—"}</span>
                    <span className="task-dep-title">{r.title}</span>
                    <span className="faint small">{r.column_type}</span>
                    {isDone({ id: r.id, key: r.key ?? null, title: r.title, column_type: r.column_type }) && (
                      <span className="task-picker-done">Done</span>
                    )}
                    <button
                      type="button"
                      className="btn small"
                      title="remove this dependency"
                      aria-label={`remove ${r.key ?? r.title}`}
                      onClick={() => unlink(r.relation_id)}
                    >
                      <X size={11} />
                    </button>
                  </div>
                ))}
                {addingDep === direction && (
                  <TaskPicker
                    placeholder={
                      direction === "blocked_by"
                        ? "which ticket blocks this one?"
                        : "which ticket does this one block?"
                    }
                    // Every type, epics included — the server drops epics
                    // unless the filter names them (MAIN-80/181).
                    types={["task", "bug", "story", "chore", "epic"]}
                    disabledIds={{
                      [task.id]: "this ticket",
                      ...Object.fromEntries(linked.map((r) => [r.id, "already linked"])),
                    }}
                    doneNote="done — won't gate anything"
                    onPick={(t) => linkBlocking(direction, t, task.id)}
                    onCancel={() => setAddingDep(null)}
                  />
                )}
              </div>
            ))}
          </div>

          {related.length > 0 && (
            <div className="task-section">
              <div className="task-section-h">related work items</div>
              <div className="task-rel-row">
                <span className="faint small">related</span>
                {related.map((r) => (
                  <span key={r.relation_id} className="task-rel">
                    <Link2 size={10} />
                    <span className="mono">{r.key ?? "—"}</span> {r.title}
                    {r.column_type === "completed" && <span className="ok"> ✓</span>}
                  </span>
                ))}
              </div>
            </div>
          )}

          {/* An epic's tickets (MAIN-83 AC-4): from the detail `children` array,
              each with a status chip (column type or archived). */}
          {isEpic && (
            <div className="task-section">
              <div className="task-section-h">
                tickets · {(children ?? []).filter((c) => c.column_type === "completed").length}/
                {(children ?? []).length}
              </div>
              {(children ?? []).length === 0 && (
                <div className="faint small">No tickets filed under this epic yet.</div>
              )}
              {(children ?? []).map((c) => (
                <div key={c.id} className="task-rel-row">
                  <span className="task-rel">
                    <span className="mono">{c.key ?? "—"}</span> {c.title}
                    <span className="backlog-status">{c.archived_at ? "archived" : c.column_type}</span>
                  </span>
                </div>
              ))}
            </div>
          )}

          <div className="task-section">
            <div className="task-section-h">activity · {comments.length} comment(s)</div>
            {comments.length === 0 && <div className="faint small">Nothing yet.</div>}
            {comments.map((c) => (
              <div key={c.id} className="task-comment">
                <div className="task-comment-head">
                  <span className="bright small">{c.author_name || "unknown"}</span>
                  {c.author_type !== "user" && (
                    <span className="faint small"> · {c.author_type}</span>
                  )}
                  <span className="faint small">
                    {" "}
                    · {new Date(c.created_at).toLocaleString()}
                  </span>
                </div>
                <Markdown src={c.body_md} />
                <AttachmentList
                  attachments={forParent("task_comment", c.id)}
                  canRemove={canRemove}
                  onRemove={(id) => void removeAttachment(id)}
                />
              </div>
            ))}

            {/* Paste is caught on the WRAPPER, not on the editor: CodeMirror
                owns its own paste handling, and an image paste has no text for
                it to insert anyway — so the file is taken here and the event
                falls through untouched for everything else (NG-6). */}
            <div
              onPaste={(e) => {
                const files = pastedFiles(e.clipboardData);
                if (files.length === 0) return;
                e.preventDefault();
                commentUploads.add(files);
              }}
            >
              <MarkdownEditor
                value={body}
                onChange={setBody}
                onSave={() => body.trim() && comment.mutate({ body_md: body.trim() })}
                placeholder="Add a comment…"
                minHeight={70}
                autoFocus={false}
              />
            </div>
            <UploadTray
              uploads={commentUploads.uploads}
              onRetry={commentUploads.retry}
              onDismiss={commentUploads.dismiss}
            />
            {pending.length > 0 && (
              <div className="attach-pending faint small">
                {pending.length} file(s) will be attached to this comment:{" "}
                {pending.map((p) => p.name).join(", ")}
              </div>
            )}
            <div style={{ display: "flex", justifyContent: "flex-end", gap: 5, marginTop: 5 }}>
              <AttachButton onFiles={commentUploads.add} />
              {/* Only on a card something actually stopped (MAIN-584 AC-12).
                  On every other card it would be a button that restarts what
                  was never stopped, and the answer to "what did that do?" is
                  "nothing you can see". */}
              {escalated && (
                <button
                  className="btn small"
                  disabled={!body.trim() || comment.isPending}
                  title="Post this ruling, clear the escalation, and let the loop pick the card up again"
                  onClick={() => comment.mutate({ body_md: body.trim(), unblock: true })}
                >
                  comment and unblock
                </button>
              )}
              <button
                className="btn small primary"
                disabled={!body.trim() || comment.isPending}
                onClick={() => comment.mutate({ body_md: body.trim() })}
              >
                {comment.isPending ? "posting…" : "comment"}
              </button>
            </div>
          </div>
        </DropZone>

        {/* ── the sidebar ── */}
        <aside className="task-side">
          {/* Status sits ABOVE the details card, not inside it: moving a task
              is an action you take, while everything below is state you read. */}
          <Select
            className="task-status"
            ariaLabel="status"
            value={task.column_id}
            onChange={moveTo}
            options={columns.map((c) => ({
              value: c.id,
              label: c.name,
              hint: c.type,
            }))}
          />

          <div className="side-card">
            <div className="side-card-h">Details</div>
            <div className="side-grid">
              <span className="faint small">Assignee</span>
              <button
                className={`task-chip ${task.assignee_user_id ? "on" : ""}`}
                onClick={() => toggleClaim(!!task.assignee_user_id)}
                title={task.assignee_user_id ? "release" : "claim"}
              >
                {task.assignee_user_id ? "claimed — release" : "unassigned — claim"}
              </button>

              {/* Above priority, because it decides whether a confined agent
                  can see this ticket at all — an unscoped task is one no
                  `/loop-build` will ever claim. */}
              <span className="faint small">Workspace</span>
              <WorkspacePicker
                ariaLabel="workspace"
                value={task.workspace_id ?? ""}
                onChange={(id) => void setWorkspace(id || NO_WORKSPACE)}
                noneLabel="— none —"
              />

              {/* Epic membership (MAIN-83 AC-4): only for a non-epic task, and
                  only when the board's epics are known. Detach with "— none —". */}
              {!isEpic && epics && (
                <>
                  <span className="faint small">Epic</span>
                  <Select
                    ariaLabel="epic"
                    value={task.parent_task_id ?? NO_EPIC}
                    onChange={setParent}
                    options={[
                      { value: NO_EPIC, label: "— none —" },
                      ...epics
                        .filter((e) => e.id !== task.id)
                        .map((e) => ({
                          value: e.id,
                          label: `${e.key ? `${e.key} ` : ""}${e.title}`,
                        })),
                    ]}
                  />
                </>
              )}

              <span className="faint small">Priority</span>
              <Select
                ariaLabel="priority"
                value={task.priority ?? 0}
                onChange={setPriority}
                options={PRIORITIES.map((p) => ({
                  value: p.value,
                  label: p.label,
                  icon: p.mark,
                  color: p.color,
                }))}
              />

              {/* Who may see this card (MAIN-103). Changing it needs tenant
                  owner/admin (MAIN-85); a refusal shows inline on the control. */}
              <span className="faint small">Visibility</span>
              <VisibilitySelect value={task.visibility} onChange={setVisibility} />

              <span className="faint small">Created</span>
              <span className="small">{new Date(task.created_at).toLocaleString()}</span>

              <span className="faint small">Updated</span>
              <span className="small">{new Date(task.updated_at).toLocaleString()}</span>

              <span className="faint small">Link</span>
              {task.url ? (
                <a className="small mono" href={task.url}>
                  {task.key}
                </a>
              ) : (
                <span className="faint small">—</span>
              )}

              <span className="faint small">ID</span>
              <span className="small mono" title={task.id}>
                {task.id.slice(0, 8)}…
              </span>
            </div>
          </div>

          <div className="side-card">
            <div className="side-card-h">Development</div>
            {task.branch || task.session_id || task.pr_url || task.worktree_path ? (
              <div className="side-grid">
                {task.branch && (
                  <>
                    <span className="faint small">Branch</span>
                    <Pill tone="info">
                      <GitBranch size={10} style={{ verticalAlign: "-1px" }} /> {task.branch}
                    </Pill>
                  </>
                )}
                {task.worktree_path && (
                  <>
                    <span className="faint small">Worktree</span>
                    <span className="small mono side-wrap" title={task.worktree_path}>
                      {task.worktree_path}
                    </span>
                  </>
                )}
                {task.session_id && (
                  <>
                    <span className="faint small">Session</span>
                    <Link className="bright small" to={`/sessions/${task.session_id}`}>
                      <SquareTerminal size={11} style={{ verticalAlign: "-2px" }} /> open
                    </Link>
                  </>
                )}
                {task.pr_url && (
                  <>
                    <span className="faint small">PR</span>
                    <div className="side-wrap">
                      <a
                        className="small"
                        href={task.pr_url}
                        target="_blank"
                        rel="noreferrer"
                      >
                        {task.pr_url.replace(/^https?:\/\//, "")} ↗
                      </a>
                      {/* The same authority the review agent has, exercised by
                          a person (MAIN-591 AC-10). It submits the comment box
                          below, because the ruling IS what the builder is sent
                          to fix — which is why it is dead until something is
                          typed there. */}
                      {canReject && (
                        <div style={{ marginTop: 4 }}>
                          <button
                            className="btn small"
                            disabled={!body.trim() || comment.isPending}
                            title={
                              body.trim()
                                ? "Post the comment below on the pull request, mark it changes-requested, and send the builder back to it"
                                : "Type the ruling in the comment box first — it is what the builder is told to fix"
                            }
                            onClick={() =>
                              comment.mutate({
                                body_md: body.trim(),
                                requestChanges: true,
                              })
                            }
                          >
                            request changes
                          </button>
                        </div>
                      )}
                    </div>
                  </>
                )}
              </div>
            ) : (
              // Named rather than hidden: "no branch" is the reason submit-PR
              // is unavailable, and this is where somebody looks for it.
              <div className="faint small">
                Nothing started. “Start work” creates a branch, a worktree and a
                session on a node.
              </div>
            )}
          </div>
        </aside>
      </div>
    </Shell>
  );
}

/**
 * The visibility control in the detail sidebar (MAIN-103): shows the current
 * visibility as a badge-button and opens a menu of the three values, PATCHing on
 * pick — mirroring `TypeSelect`. The trigger and each option are buttons, so it
 * is keyboard-reachable; the menu is portalled for the same reason. The MAIN-85
 * gate's 403 surfaces INLINE, below the control (a 404 is handled by the caller,
 * which closes the modal).
 */
function VisibilitySelect({
  value,
  onChange,
}: {
  value: string | null | undefined;
  onChange: (visibility: string) => Promise<{ ok: boolean; status: number }>;
}) {
  const [open, setOpen] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const close = useCallback(() => setOpen(false), []);
  const { hostRef, portal } = useAnchoredMenu(open, close, {
    height: VISIBILITY_META.length * 34 + 42,
  });
  const current = value ?? "team";
  const cur = VISIBILITY_META.find((v) => v.value === current) ?? VISIBILITY_META[1];
  const pick = async (v: string) => {
    setOpen(false);
    if (v === current) return;
    setError(null);
    const res = await onChange(v);
    // The MAIN-85 gate: say why, right here, rather than swallowing it.
    if (!res.ok && res.status === 403) {
      setError("this needs tenant owner or admin");
    }
  };
  const menu = portal(
    <div className="type-menu">
      <div className="type-menu-head">Change visibility</div>
      {VISIBILITY_META.map((v) => (
        <button
          key={v.value}
          className={`type-menu-item ${v.tone}${v.value === current ? " current" : ""}`}
          title={v.tooltip}
          onClick={() => void pick(v.value)}
        >
          <v.Icon size={14} className="type-menu-icon" />
          <span className="type-menu-label">{v.label}</span>
        </button>
      ))}
    </div>,
    "type-menu-portal",
  );
  return (
    <div ref={hostRef} className="task-vis-field">
      <button
        className={`type-select-trigger ${cur.tone}`}
        aria-label={`visibility: ${cur.label}`}
        title={cur.tooltip}
        aria-haspopup="menu"
        onClick={() => setOpen((v) => !v)}
      >
        <cur.Icon size={14} />
        <span className="type-menu-label">{cur.label}</span>
        <ChevronDown size={11} className="type-select-caret" />
      </button>
      {menu}
      {error && (
        <span className="small" style={{ color: "var(--nook-err)", display: "block", marginTop: 3 }}>
          {error}
        </span>
      )}
    </div>
  );
}

/**
 * Labels as removable chips plus a picker, the way every tracker does it.
 *
 * The previous version listed every label in the tenant as a toggle, which
 * reads fine at two labels and becomes a wall at twenty — and gave no visual
 * answer to "what is on this task?" without comparing highlighted states.
 */
function LabelField({
  all,
  on,
  onToggle,
  onCreate,
}: {
  all: TaskLabel[];
  on: TaskLabel[];
  onToggle: (label: TaskLabel, add: boolean) => void;
  onCreate: (name: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const attached = new Set(on.map((l) => l.name));
  const available = all.filter((l) => !attached.has(l.name));

  const q = query.trim();
  const ql = q.toLowerCase();
  const filtered = q ? available.filter((l) => l.name.toLowerCase().includes(ql)) : available;
  // Offer "create" only when the typed name matches no existing label at all
  // (attached or not) — otherwise you'd get a create button for a label that
  // already exists and just needs attaching.
  const canCreate = q.length > 0 && !all.some((l) => l.name.toLowerCase() === ql);

  const reset = () => {
    setQuery("");
    setOpen(false);
  };

  // Portalled for the same reason the selects are: this sits inside
  // `.task-main`, which scrolls, inside `.modal`, which hides its overflow.
  const close = useCallback(() => {
    setOpen(false);
    setQuery("");
  }, []);
  const { hostRef, portal } = useAnchoredMenu(open, close, {
    height: Math.min((filtered.length + 2) * 26 + 8, 260),
  });

  const menu = portal(
    <>
      <input
        className="label-search"
        autoFocus
        placeholder="filter or type a new label…"
        value={query}
        onChange={(e) => setQuery(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.stopPropagation();
            reset();
          } else if (e.key === "Enter") {
            e.preventDefault();
            // Enter takes the single obvious action: attach the one match, or
            // create the new name. Ambiguous (several matches) does nothing —
            // pick one with the mouse.
            if (filtered.length === 1) {
              onToggle(filtered[0], true);
              reset();
            } else if (canCreate) {
              onCreate(q);
              reset();
            }
          }
        }}
      />
      {filtered.map((l) => (
        <button
          key={l.id}
          className="ctx-item"
          onClick={() => {
            onToggle(l, true);
            reset();
          }}
        >
          <span style={{ color: l.color }}>{l.name}</span>
        </button>
      ))}
      {canCreate && (
        <button
          className="ctx-item label-create"
          onClick={() => {
            onCreate(q);
            reset();
          }}
        >
          ＋ Create “{q}”
        </button>
      )}
      {filtered.length === 0 && !canCreate && (
        <div className="faint small" style={{ padding: "4px 8px" }}>
          {available.length === 0 ? "All labels are on this task." : "No match."}
        </div>
      )}
    </>,
    "label-menu",
  );

  return (
    <div className="task-labels-row">
      <span className="faint small">Labels</span>
      <div className="task-labels-field">
        {on.map((l) => (
          <span
            key={l.id}
            className="label-chip"
            style={{ borderColor: l.color, color: l.color }}
          >
            {l.name}
            <button
              className="label-x"
              onClick={() => onToggle(l, false)}
              title={`remove ${l.name}`}
            >
              ×
            </button>
          </span>
        ))}
        <div ref={hostRef} className="label-picker">
          <button className="label-add" onClick={() => setOpen((v) => !v)}>
            + label
          </button>
          {menu}
        </div>
      </div>
    </div>
  );
}

/** Backdrop + panel + Escape, shared by the loading and loaded states. */
function Shell({
  children,
  onClose,
}: {
  children: React.ReactNode;
  onClose: () => void;
}) {
  useEffect(() => {
    const esc = (e: KeyboardEvent) => {
      // Only when nothing is being typed into — Escape inside the editor means
      // "cancel this edit", and closing the whole modal would throw the draft
      // away with it.
      const tag = (document.activeElement?.tagName ?? "").toLowerCase();
      if (e.key === "Escape" && tag !== "textarea" && tag !== "input") onClose();
    };
    window.addEventListener("keydown", esc);
    return () => window.removeEventListener("keydown", esc);
  }, [onClose]);

  return (
    <div className="modal-backdrop" onMouseDown={onClose}>
      <div className="modal task-modal" onMouseDown={(e) => e.stopPropagation()}>
        {children}
      </div>
    </div>
  );
}
