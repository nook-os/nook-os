// Filing work from the board, without a ticket first (MAIN-364).
//
// The old path made you produce the thing you were trying to produce: file a
// stub ticket, open it, find "Draft a spec", and only then describe what you
// actually wanted. Every PM read that as a bug, and they were right — the spec
// loop's whole job is turning an idea into a ticket, so requiring a ticket to
// start one is backwards.
//
// So this is one gesture: describe the idea, say which workspace it belongs to,
// and it lands in triage. `Draft with AI` (the default) additionally starts the
// loop with your text as its seed — the ticket you get back is the one the
// loop wrote, not the stub you typed. `File it myself` is the escape hatch for
// when you already know exactly what you want and the interview is noise.
//
// Epic is just another type here, which is what makes "a button to create an
// epic" fall out for free: an epic + AI routes to the decomposer rather than
// the spec interview, because `loopAction` already keys on the type.
import React, { useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Sparkles, X } from "lucide-react";
import { api } from "@nookos/api";
import { Select, TypeSelect } from "@nookos/ui";
import { createLoopJob, loopAction } from "./loop";

/** Where a freshly-filed idea belongs: the refinement queue, by SEMANTIC name.
 *  `column_type` exists precisely so a caller can say "the backlog" without
 *  knowing which uuid that is on this board today. */
const TRIAGE: string = "backlog";

export interface NewTicketResult {
  taskId: string;
  /** A loop was started on it — the caller opens the loop rather than the card. */
  drafting: boolean;
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
  const [idea, setIdea] = useState("");
  const [type, setType] = useState(initialType);
  const [workspaceId, setWorkspaceId] = useState<string>("");
  const [useAi, setUseAi] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const ref = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    ref.current?.focus();
  }, []);

  const { data: workspaces } = useQuery({
    queryKey: ["workspaces"],
    queryFn: async () => (await api.GET("/api/v1/workspaces")).data ?? [],
  });

  // Preselect when there is exactly one: with a single repo the question has
  // only one answer, and making somebody answer it anyway is ceremony.
  useEffect(() => {
    if (!workspaceId && workspaces?.length === 1) setWorkspaceId(workspaces[0].id);
  }, [workspaces, workspaceId]);

  const options = useMemo(
    () => [
      { value: "", label: "No workspace" },
      ...(workspaces ?? []).map((w) => ({ value: w.id, label: w.name })),
    ],
    [workspaces],
  );

  // What the AI run would be. An epic decomposes; everything else gets the spec
  // interview. Derived from the SAME helper the ticket page uses, so the button
  // cannot promise a different run from the one that starts.
  const action = loopAction(type, undefined);

  const submit = async () => {
    const text = idea.trim();
    if (!text || busy) return;
    setBusy(true);
    setError(null);

    // The title is the first line; anything after it is already description.
    // Somebody pasting three paragraphs should not get a three-paragraph title.
    const [first, ...rest] = text.split("\n");
    const title = first.trim().slice(0, 200);
    const description = rest.join("\n").trim();

    const created = await api.POST("/api/v1/boards/{id}/tasks", {
      params: { path: { id: boardId } },
      body: {
        title,
        column_type: TRIAGE,
        type,
        ...(description ? { description } : {}),
        ...(workspaceId ? { workspace_id: workspaceId } : {}),
      },
    });
    const taskId = created.data?.id;
    if (!taskId) {
      // The global write-failure toast already says WHY (method, path, server
      // message); this only has to say the modal is still holding your text.
      setBusy(false);
      setError("Could not file that — your text is still here.");
      return;
    }

    if (!useAi) {
      setBusy(false);
      onCreated({ taskId, drafting: false });
      return;
    }

    // The ticket exists either way — a loop that cannot start is a ticket in
    // triage, not a lost idea. So a failure here reports itself and still hands
    // the caller the card, rather than rolling back work the person did.
    const job = await createLoopJob(action.kind, taskId, text);
    setBusy(false);
    if (!job.data) {
      // Worth its own sentence rather than the generic toast: the ticket DID
      // get filed, so "it failed" would send someone off to re-file it.
      setError("Filed it in triage, but the draft could not start.");
      return;
    }
    onCreated({ taskId, drafting: true });
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div
        className="modal dialog"
        role="dialog"
        aria-modal="true"
        aria-label="New work"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="modal-header">
          <span>New work</span>
          <button className="btn small" onClick={onClose} aria-label="close">
            <X size={12} />
          </button>
        </div>

        <div className="modal-body">
          <label className="field">
            <span className="field-label">What do you want?</span>
            <textarea
              ref={ref}
              className="composer-input"
              rows={4}
              placeholder="Describe the idea. The first line becomes the title."
              value={idea}
              onChange={(e) => setIdea(e.target.value)}
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
              <Select
                value={workspaceId}
                options={options}
                onChange={setWorkspaceId}
                ariaLabel="Workspace"
              />
            </label>
          </div>

          <div className="field">
            <span className="field-label">How</span>
            <div className="seg" role="radiogroup" aria-label="How to file it">
              <button
                type="button"
                role="radio"
                aria-checked={useAi}
                className={`btn small${useAi ? " primary" : ""}`}
                onClick={() => setUseAi(true)}
              >
                <Sparkles size={11} /> Draft with AI
              </button>
              <button
                type="button"
                role="radio"
                aria-checked={!useAi}
                className={`btn small${!useAi ? " primary" : ""}`}
                onClick={() => setUseAi(false)}
              >
                File it myself
              </button>
            </div>
            <span className="faint small">
              {useAi
                ? `Files it in triage and starts ${action.label.toLowerCase()} from what you wrote.`
                : "Files it in triage exactly as written."}
            </span>
          </div>

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
            disabled={!idea.trim() || busy}
          >
            {busy ? "Working…" : useAi ? "Draft it" : "File it"}
          </button>
        </div>
      </div>
    </div>
  );
}
