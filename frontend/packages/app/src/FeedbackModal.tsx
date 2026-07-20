// Quick capture for "this should be better".
//
// A thought about the tool you are using arrives while you are using it, and
// routing to a page to write it down is enough friction to lose it. This opens
// over whatever you were doing, takes the sentence, and gets out of the way —
// the rolling log on the Feedback page is where it goes to be read later.
import React, { useEffect, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { create } from "zustand";
import { GitBranch, Send, X } from "lucide-react";
import { api } from "@nookos/api";
import { Pill } from "@nookos/ui";
import { askChoice, askText, notify } from "./dialogs";

interface FeedbackModalState {
  open: boolean;
  show(): void;
  hide(): void;
}

export const useFeedbackModal = create<FeedbackModalState>((set) => ({
  open: false,
  show: () => set({ open: true }),
  hide: () => set({ open: false }),
}));

export function FeedbackModalHost() {
  const open = useFeedbackModal((s) => s.open);
  return open ? <FeedbackModal /> : null;
}

function FeedbackModal() {
  const hide = useFeedbackModal((s) => s.hide);
  const queryClient = useQueryClient();
  const [body, setBody] = useState("");
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<string | null>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);

  const { data: target, refetch: refetchTarget } = useQuery({
    queryKey: ["feedback", "target"],
    queryFn: async () => (await api.GET("/api/v1/feedback/target", {})).data,
  });
  const { data: workspaces } = useQuery({
    queryKey: ["workspaces"],
    queryFn: async () => (await api.GET("/api/v1/workspaces")).data ?? [],
  });

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  /** Choose the repo and branch improvements land on. Changeable at any time. */
  const configure = async () => {
    const choices = (workspaces ?? [])
      .filter((w) => w.locations.length > 0)
      .map((w) => ({
        value: w.id,
        label: w.name,
        description: w.locations.map((l) => l.node_name).join(", "),
      }));
    if (choices.length === 0) {
      await notify(
        "No workspace available",
        "Clone or import a repository first — feedback is worked on inside one.",
      );
      return;
    }
    const workspaceId = await askChoice({
      title: "Which repo should feedback improve?",
      description:
        "A session named “Feedback” runs there and picks up everything you send, so context accumulates instead of being re-explained.",
      choices,
      confirmLabel: "use this repo",
    });
    if (!workspaceId) return;

    const branch = await askText({
      title: "Which branch should the work land on?",
      description:
        "Improvements are committed here, so they stay isolated from your main line and can deploy to a dev environment. Leave empty to let the agent name a branch per change.",
      label: "Branch",
      value: target?.branch ?? "",
      placeholder: "feature/my-dev-branch",
      confirmLabel: "save target",
    });

    // What to do with a finished change. Left empty this falls back to
    // .nook-feedback.md in the repo, then to the built-in wording — so a
    // project can carry its own rules without anyone setting them here.
    const instructions = await askText({
      title: "What should the agent do when it's done?",
      description:
        "Commit and push? Open a PR? Leave it uncommitted for review? Replaces the default wording. Leave empty to use .nook-feedback.md from the repo, or the default if there isn't one.",
      label: "When finished",
      value: target?.instructions ?? "",
      multiline: true,
      placeholder: "Commit, push, and open a PR against main. Never push to main directly.",
      confirmLabel: "save target",
    });

    const { error } = await api.PUT("/api/v1/feedback/target", {
      body: {
        workspace_id: workspaceId,
        branch: branch ?? null,
        instructions: instructions ?? null,
      },
    });
    if (error) {
      await notify("Could not save that target", JSON.stringify(error));
      return;
    }
    refetchTarget();
  };

  const submit = async () => {
    const text = body.trim();
    if (!text) return;
    if (!target?.configured) {
      await configure();
      return; // let them confirm the target before sending
    }

    setBusy(true);
    setStatus("sending to the feedback session…");
    const { data, error, response } = await api.POST("/api/v1/feedback", {
      body: { body: text, workspace_id: null, runtime: null },
    });
    setBusy(false);
    if (error || !response.ok) {
      setStatus(null);
      await notify("Could not queue that", JSON.stringify(error));
      return;
    }
    // The server says whether it actually reached the session, rather than
    // leaving it looking queued when nothing is working on it.
    if (data?.status === "dropped") {
      setStatus(null);
      await notify(
        "Queued, but not delivered",
        "The feedback session could not be reached — its node may be offline. It's saved in the log; send it again once the node is back.",
      );
    }
    setBody("");
    queryClient.invalidateQueries({ queryKey: ["feedback"] });
    queryClient.invalidateQueries({ queryKey: ["sessions"] });
    hide();
  };

  return (
    <div className="modal-backdrop" onMouseDown={hide}>
      <div
        className="modal"
        style={{ width: 560 }}
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key === "Escape") hide();
          // Enter submits; the body is usually one sentence, and a newline is
          // still available on shift.
          if (e.key === "Enter" && (e.metaKey || e.ctrlKey || !e.shiftKey)) {
            e.preventDefault();
            void submit();
          }
        }}
      >
        <div className="modal-header">What should be better?</div>
        <div className="modal-body">
          <textarea
            ref={inputRef}
            className="input mono small"
            rows={5}
            placeholder="The terminal should remember its scroll position…"
            value={body}
            onChange={(e) => setBody(e.target.value)}
          />
          <div
            style={{
              display: "flex",
              alignItems: "center",
              gap: 8,
              marginTop: 8,
              flexWrap: "wrap",
            }}
          >
            {target?.configured ? (
              <>
                <Pill tone="accent">{target.workspace_name}</Pill>
                <Pill tone={target.branch ? "info" : "dim"}>
                  <GitBranch size={11} /> {target.branch ?? "agent picks a branch"}
                </Pill>
              </>
            ) : (
              <Pill tone="warn">no target set</Pill>
            )}
            <button className="btn small" onClick={configure} disabled={busy}>
              change
            </button>
            {status && <span className="muted small">{status}</span>}
          </div>
        </div>
        <div className="modal-footer">
          <button className="btn primary" onClick={submit} disabled={busy || !body.trim()}>
            <Send size={13} /> {busy ? "sending…" : "send"}
          </button>
          <button className="btn" onClick={hide}>
            <X size={13} /> cancel
          </button>
          <span className="faint small" style={{ marginLeft: "auto" }}>
            enter sends · shift+enter for a newline
          </span>
        </div>
      </div>
    </div>
  );
}
