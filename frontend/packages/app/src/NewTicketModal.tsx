// Starting a spec loop from the board, without a ticket first (MAIN-364).
//
// The old path made you produce the thing you were trying to produce: file a
// stub ticket, open it, find "Draft a spec", and only then describe what you
// actually wanted. Every PM read that as a bug, and they were right — the spec
// loop's whole job is turning an idea into a ticket, so requiring a ticket to
// start one is backwards.
//
// This modal is therefore AI-only, on purpose. Filing a ticket by hand already
// has a home — the per-column composer, one click away on the board itself —
// and duplicating it here would only make people choose between two things that
// look the same. This is the other job: hand an agent a prompt and let it come
// back with the ticket.
//
// What you type is the PROMPT, not a title. It goes to the agent verbatim as
// the run's seed; the ticket it creates is a placeholder the loop rewrites, so
// splitting the text into title-and-body would be inventing structure the agent
// is about to replace — and worse, it would quietly drop everything after the
// first line into a description nobody asked for.
//
// Epic is just another type here, which is what makes "New epic" fall out for
// free: an epic routes to the decomposer rather than the spec interview,
// because `loopAction` already keys on the type.
import React, { useEffect, useRef, useState } from "react";
import { Sparkles, X } from "lucide-react";
import { api } from "@nookos/api";
import { TypeSelect } from "@nookos/ui";
import { createLoopJob, loopAction } from "./loop";
import { WorkspacePicker } from "./WorkspacePicker";
import { useWorkspaces } from "./workspaces";

/** Where a freshly-filed idea belongs: the refinement queue, by SEMANTIC name.
 *  `column_type` exists precisely so a caller can say "the backlog" without
 *  knowing which uuid that is on this board today. */
const TRIAGE: string = "backlog";

/** A title has to exist for the card to render before the agent has written
 *  anything, so the prompt's opening words stand in. Deliberately NOT parsed
 *  out of the prompt as a real title — the loop replaces it. */
function placeholderTitle(prompt: string): string {
  const oneLine = prompt.replace(/\s+/g, " ").trim();
  return oneLine.length > 80 ? `${oneLine.slice(0, 79)}…` : oneLine;
}

export interface NewTicketResult {
  taskId: string;
}

export function NewTicketModal({
  boardId,
  onClose,
  onCreated,
  /** Preselects the type. The board's "New epic" entry point passes `epic`. */
  initialType = "task",
}: {
  boardId: string;
  onClose: () => void;
  onCreated: (result: NewTicketResult) => void;
  initialType?: string;
}) {
  const [prompt, setPrompt] = useState("");
  const [type, setType] = useState(initialType);
  const [workspaceId, setWorkspaceId] = useState<string>("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const ref = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    ref.current?.focus();
  }, []);

  // Preselect when there is exactly one: with a single repo the question has
  // only one answer, and making somebody answer it anyway is ceremony. Asking
  // for two rows is how "exactly one" stays answerable now that the collection
  // is paged (MAIN-606) — a full page would only say "at least fifty".
  const firstTwo = useWorkspaces({ limit: 2 });
  useEffect(() => {
    if (!workspaceId && !firstTwo.hasMore && firstTwo.rows.length === 1)
      setWorkspaceId(firstTwo.rows[0].id);
  }, [firstTwo.hasMore, firstTwo.rows, workspaceId]);

  // Which run this will be. Derived from the SAME helper the ticket page uses,
  // so the button cannot promise a different run from the one that starts.
  const action = loopAction(type, undefined);

  const submit = async () => {
    const text = prompt.trim();
    if (!text || busy) return;
    setBusy(true);
    setError(null);

    const created = await api.POST("/api/v1/boards/{id}/tasks", {
      params: { path: { id: boardId } },
      body: {
        title: placeholderTitle(text),
        column_type: TRIAGE,
        type,
        ...(workspaceId ? { workspace_id: workspaceId } : {}),
      },
    });
    const taskId = created.data?.id;
    if (!taskId) {
      // The global write-failure toast already says WHY (method, path, server
      // message); this only has to say the modal is still holding your text.
      setBusy(false);
      setError("Could not file that — your prompt is still here.");
      return;
    }

    // The prompt goes to the agent WHOLE. The ticket exists either way, so a
    // loop that cannot start is a ticket in triage, not a lost idea.
    const job = await createLoopJob(action.kind, taskId, text);
    setBusy(false);
    if (!job.data) {
      setError("Filed it in triage, but the agent could not start.");
      return;
    }
    onCreated({ taskId });
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div
        className="modal dialog"
        role="dialog"
        aria-modal="true"
        aria-label="Draft with AI"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="modal-header">
          <span>
            <Sparkles size={12} /> Draft with AI
          </span>
          <button className="btn small" onClick={onClose} aria-label="close">
            <X size={12} />
          </button>
        </div>

        <div className="modal-body">
          <label className="field">
            <span className="field-label">What should it build?</span>
            <textarea
              ref={ref}
              className="composer-input"
              rows={5}
              placeholder="Describe what you want. This is the prompt the agent starts from."
              value={prompt}
              onChange={(e) => setPrompt(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                  e.preventDefault();
                  void submit();
                }
              }}
            />
          </label>

          <div className="side-grid">
            <label className="field">
              <span className="field-label">Type</span>
              <TypeSelect value={type} onChange={setType} ariaLabel="Issue type" />
            </label>
            <label className="field">
              <span className="field-label">Workspace</span>
              <WorkspacePicker
                value={workspaceId}
                onChange={setWorkspaceId}
                noneLabel="No workspace"
                ariaLabel="Workspace"
              />
            </label>
          </div>

          <span className="faint small">
            Files a placeholder in triage and starts {action.label.toLowerCase()}.
            The agent writes the ticket.
          </span>

          {error && (
            <div className="small" role="alert" style={{ color: "var(--nook-err)" }}>
              {error}
            </div>
          )}
        </div>

        <div className="modal-footer">
          <button className="btn small" onClick={onClose}>
            Cancel
          </button>
          <button
            className="btn small primary"
            onClick={() => void submit()}
            disabled={!prompt.trim() || busy}
          >
            <Sparkles size={11} /> {busy ? "Starting…" : "Start drafting"}
          </button>
        </div>
      </div>
    </div>
  );
}
