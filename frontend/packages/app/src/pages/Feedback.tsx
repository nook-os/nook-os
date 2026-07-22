// Feedback: say what should be better, and it lands in a working session.
//
// The expensive part of asking for a change is re-explaining the project every
// time. Feedback is queued against one workspace and typed into a single
// long-lived session, so context accumulates there instead. The log below is
// the rolling record of what was asked and what came of it.
import React, { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import { ExternalLink, GitPullRequest, Send, SquareTerminal } from "lucide-react";
import { api } from "@nookos/api";
import { Empty, Panel, Pill } from "@nookos/ui";
import { askChoice, askText, notify } from "../dialogs";

/** Where forks send their improvements. */
const UPSTREAM = "https://github.com/nook-os/nook-os";

function statusTone(status: string) {
  if (status === "submitted") return "ok" as const;
  if (status === "delivered") return "info" as const;
  if (status === "dropped") return "err" as const;
  return "warn" as const;
}

/** GitHub compare link for a branch, from the workspace's own remote. */
function compareUrl(remote: string | null | undefined): string {
  if (!remote) return `${UPSTREAM}/compare`;
  const https = `https://${remote.replace(/^https?:\/\//, "")}`;
  return `${https}/compare`;
}

export function FeedbackPage() {
  const queryClient = useQueryClient();
  const [body, setBody] = useState("");
  const [busy, setBusy] = useState(false);

  const { data: target } = useQuery({
    queryKey: ["feedback", "target"],
    queryFn: async () => (await api.GET("/api/v1/feedback/target", {})).data,
  });
  const { data: items } = useQuery({
    queryKey: ["feedback"],
    queryFn: async () => (await api.GET("/api/v1/feedback")).data ?? [],
    refetchInterval: 10000,
  });
  const { data: workspaces } = useQuery({
    queryKey: ["workspaces"],
    queryFn: async () => (await api.GET("/api/v1/workspaces")).data ?? [],
  });

  /** First run: nothing is configured, so ask which repo this improves. */
  const chooseWorkspace = async (): Promise<string | null> => {
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
      return null;
    }
    return askChoice({
      title: "Where should feedback be worked on?",
      description:
        "A session named “Feedback” runs in this repo and picks up everything you send. You can change it later by submitting from another workspace.",
      choices,
      confirmLabel: "use this repo",
    });
  };

  const submit = async () => {
    const text = body.trim();
    if (!text) return;
    let workspaceId: string | undefined;
    if (!target?.configured) {
      const picked = await chooseWorkspace();
      if (!picked) return;
      workspaceId = picked;
    }

    setBusy(true);
    const { error, response } = await api.POST("/api/v1/feedback", {
      body: { body: text, workspace_id: workspaceId ?? null, runtime: null },
    });
    setBusy(false);
    if (error || !response.ok) {
      await notify("Could not queue that", JSON.stringify(error));
      return;
    }
    setBody("");
    queryClient.invalidateQueries({ queryKey: ["feedback"] });
    queryClient.invalidateQueries({ queryKey: ["sessions"] });
  };

  const markSubmitted = async (id: string, url: string) => {
    await api.PATCH("/api/v1/feedback/{id}", {
      params: { path: { id } },
      body: { status: "submitted", pr_url: url },
    });
    queryClient.invalidateQueries({ queryKey: ["feedback"] });
  };

  return (
    <div className="nook-grid" style={{ gridTemplateColumns: "1fr 1fr" }}>
      <Panel
        title="Send feedback"
        actions={
          target?.configured ? (
            <Pill tone="accent">{target.workspace_name}</Pill>
          ) : (
            <Pill tone="warn">not set up</Pill>
          )
        }
      >
        <div style={{ display: "flex", flexDirection: "column", height: "100%" }}>
          <textarea
            className="input mono small"
            style={{
              flex: 1,
              resize: "none",
              border: "none",
              borderRadius: 0,
              background: "var(--nook-bg)",
              padding: 10,
            }}
            placeholder={
              "What should be better?\n\n" +
              "e.g. The sessions list should remember my last filter.\n" +
              "     Cloning should show which node it picked."
            }
            value={body}
            onChange={(e) => setBody(e.target.value)}
            onKeyDown={(e) => {
              // ⌘/Ctrl+Enter sends, like every other compose box.
              if ((e.metaKey || e.ctrlKey) && e.key === "Enter") submit();
            }}
          />
          <div
            style={{
              display: "flex",
              gap: 8,
              alignItems: "center",
              padding: 8,
              borderTop: "1px solid var(--nook-border)",
            }}
          >
            <button
              className="btn primary small"
              onClick={submit}
              disabled={busy || !body.trim()}
            >
              <Send size={12} /> {busy ? "sending…" : "send to session"}
            </button>
            <a
              className="btn small"
              href={compareUrl(target?.git_remote)}
              target="_blank"
              rel="noreferrer"
              title="open a pull request from your fork"
            >
              <GitPullRequest size={12} /> open PR
            </a>
            <span className="faint small" style={{ marginLeft: "auto" }}>
              ⌘↵ to send · upstream{" "}
              <a href={UPSTREAM} target="_blank" rel="noreferrer">
                nook-os/nook-os
              </a>
            </span>
          </div>
        </div>
      </Panel>

      <Panel title={`Rolling log (${(items ?? []).length})`}>
        {(items ?? []).length === 0 ? (
          <Empty>
            Nothing yet. Whatever you send lands in a session named “
            {target?.session_name ?? "Feedback"}” and stays in this log.
          </Empty>
        ) : (
          <table className="nook-table">
            <tbody>
              {(items ?? []).map((f) => (
                <tr key={f.id}>
                  <td style={{ whiteSpace: "normal" }}>
                    <div className="bright">{f.body}</div>
                    <div
                      className="faint small"
                      style={{ display: "flex", gap: 8, marginTop: 3 }}
                    >
                      <span>{new Date(f.created_at).toLocaleString()}</span>
                      {f.session_id && (
                        <Link to={`/sessions/${f.session_id}`}>
                          <SquareTerminal size={10} style={{ verticalAlign: "-1px" }} />{" "}
                          session
                        </Link>
                      )}
                      {f.pr_url && (
                        <a href={f.pr_url} target="_blank" rel="noreferrer">
                          <ExternalLink size={10} style={{ verticalAlign: "-1px" }} /> PR
                        </a>
                      )}
                    </div>
                  </td>
                  <td style={{ width: 90 }}>
                    <Pill tone={statusTone(f.status)}>{f.status}</Pill>
                  </td>
                  <td style={{ width: 40 }}>
                    {f.status !== "submitted" && (
                      <button
                        className="btn small icon"
                        title="record the PR this became"
                        onClick={async () => {
                          const url = await askText({
                            title: "Record the pull request",
                            description:
                              "Paste the PR this feedback became — it shows in the log so the trail stays complete.",
                            label: "Pull request URL",
                            placeholder: `${UPSTREAM}/pull/123`,
                            confirmLabel: "record",
                          });
                          if (url) markSubmitted(f.id, url);
                        }}
                      >
                        <GitPullRequest size={11} />
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Panel>
    </div>
  );
}
