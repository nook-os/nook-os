// The task modal: one whole issue, opened over the board.
//
// This was a split pane. A modal wins for the reason Jira and Linear both use
// one: a task body is a spec, and reading it in a 420px column beside four
// other columns meant every line wrapped twice. The board is one keypress away
// and the work here is reading and writing prose, not comparing cards.
import React, { useCallback, useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import { GitBranch, SquareTerminal, X, Ban, Link2, MoreHorizontal } from "lucide-react";
import { api, type TaskLabel, type RelatedTask } from "@nookos/api";
import {
  Pill,
  Markdown,
  MarkdownEditor,
  EditableMarkdown,
  Select,
  useAnchoredMenu,
} from "@nookos/ui";
import { PRIORITIES } from "./taskmeta";

/** The "no workspace" option's value. `Select` needs a string, and an empty
 *  one cannot collide with a uuid. */
const NO_WORKSPACE = "";


export function TaskDetail({
  taskId,
  columns,
  onClose,
  onMenu,
}: {
  taskId: string;
  /** The board's columns, so state can be changed from here. */
  columns: { id: string; name: string; type?: string }[];
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
  const { data: workspaces } = useQuery({
    queryKey: ["workspaces"],
    queryFn: async () => (await api.GET("/api/v1/workspaces")).data ?? [],
  });

  const bust = () => {
    qc.invalidateQueries({ queryKey: ["task", taskId] });
    qc.invalidateQueries({ queryKey: ["boards"] });
  };

  const comment = useMutation({
    mutationFn: async (body_md: string) => {
      await api.POST("/api/v1/tasks/{id}/comments", {
        params: { path: { id: taskId } },
        body: { body_md },
      });
    },
    onSuccess: () => {
      setBody("");
      bust();
    },
  });

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
    await api.PATCH("/api/v1/tasks/{id}", {
      params: { path: { id: taskId } },
      body: { description },
    });
    setEditing(false);
    bust();
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

  if (isLoading || !data) {
    return (
      <Shell onClose={onClose}>
        <div className="faint small" style={{ padding: 16 }}>
          Loading…
        </div>
      </Shell>
    );
  }

  const { task, comments, blocked_by, blocking, related, is_blocked } = data;
  const linked = [...blocked_by, ...blocking, ...related];

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
        <div className="task-main">
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
              placeholder="No description yet — double-click to write the acceptance criteria."
            />
          </div>

          {linked.length > 0 && (
            <div className="task-section">
              <div className="task-section-h">linked work items</div>
              {(
                [
                  ["blocked by", blocked_by],
                  ["blocking", blocking],
                  ["related", related],
                ] as [string, RelatedTask[]][]
              ).map(([label, list]) =>
                list.length === 0 ? null : (
                  <div key={label} className="task-rel-row">
                    <span className="faint small">{label}</span>
                    {list.map((r) => (
                      <span key={r.relation_id} className="task-rel">
                        <Link2 size={10} />
                        <span className="mono">{r.key ?? "—"}</span> {r.title}
                        {r.column_type === "completed" && <span className="ok"> ✓</span>}
                      </span>
                    ))}
                  </div>
                ),
              )}
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
              </div>
            ))}

            <MarkdownEditor
              value={body}
              onChange={setBody}
              onSave={() => body.trim() && comment.mutate(body.trim())}
              placeholder="Add a comment…"
              minHeight={70}
              autoFocus={false}
            />
            <div style={{ display: "flex", justifyContent: "flex-end", marginTop: 5 }}>
              <button
                className="btn small primary"
                disabled={!body.trim() || comment.isPending}
                onClick={() => comment.mutate(body.trim())}
              >
                {comment.isPending ? "posting…" : "comment"}
              </button>
            </div>
          </div>
        </div>

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
              <Select
                ariaLabel="workspace"
                value={task.workspace_id ?? NO_WORKSPACE}
                onChange={setWorkspace}
                options={[
                  { value: NO_WORKSPACE, label: "— none —" },
                  ...(workspaces ?? []).map((w) => ({
                    value: w.id,
                    label: w.name,
                  })),
                ]}
              />

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
                    <a
                      className="small side-wrap"
                      href={task.pr_url}
                      target="_blank"
                      rel="noreferrer"
                    >
                      {task.pr_url.replace(/^https?:\/\//, "")} ↗
                    </a>
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
