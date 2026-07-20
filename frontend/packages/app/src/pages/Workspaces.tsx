import React, { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useParams } from "react-router-dom";
import { Plus, Trash2 } from "lucide-react";
import { api } from "@nookos/api";
import { Empty, Panel, Pill, StatusDot, statusTone } from "@nookos/ui";
import { ActivityFeed } from "./Activity";
import { NotesPanel } from "./Notes";
import { useNewWork } from "../newwork";
import { WorkspaceLocations } from "../WorkspaceLocations";

export function WorkspacesPage() {
  const showNewWork = useNewWork((s) => s.show);
  const { data: workspaces } = useQuery({
    queryKey: ["workspaces"],
    queryFn: async () => (await api.GET("/api/v1/workspaces")).data ?? [],
  });

  return (
    <div className="nook-grid" style={{ gridTemplateColumns: "1fr" }}>
      <Panel
        title={`Workspaces (${(workspaces ?? []).length})`}
        actions={
          <button className="btn primary small" onClick={() => showNewWork()}>
            <Plus size={12} /> New Work
          </button>
        }
      >
        {(workspaces ?? []).length === 0 ? (
          <Empty>
            No workspaces yet. Hit <b>+ New Work</b> to clone a repo or start a
            new project — or join a node and its repositories appear here.
          </Empty>
        ) : (
          <table className="nook-table">
            <thead>
              <tr>
                <th style={{ width: "28%" }}>Workspace</th>
                <th>Where it lives</th>
                <th style={{ width: 40 }} />
              </tr>
            </thead>
            <tbody>
              {(workspaces ?? []).map((w) => (
                <tr key={w.id}>
                  <td>
                    <Link className="bright" to={`/workspaces/${w.id}`}>
                      {w.name}
                    </Link>
                  </td>
                  <td>
                    <WorkspaceLocations locations={w.locations} />
                  </td>
                  <td>
                    <DeleteWorkspaceButton
                      id={w.id}
                      name={w.name}
                      checkouts={w.locations.length}
                    />
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

function EnvPanel({ workspaceId }: { workspaceId: string }) {
  const [content, setContent] = useState<string | null>(null);
  const [status, setStatus] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const { data: loaded } = useQuery({
    queryKey: ["secrets", workspaceId, ".env"],
    queryFn: async () => {
      const { data, response } = await api.GET(
        "/api/v1/workspaces/{id}/secrets/{name}",
        { params: { path: { id: workspaceId, name: ".env" } } },
      );
      if (response.status === 404) return { content: "" };
      return { content: data?.content ?? "" };
    },
    retry: false,
  });
  const value = content ?? loaded?.content ?? "";

  const save = async () => {
    setBusy(true);
    const { data, error } = await api.PUT(
      "/api/v1/workspaces/{id}/secrets/{name}",
      {
        params: { path: { id: workspaceId, name: ".env" } },
        body: { content: value },
      },
    );
    setBusy(false);
    setStatus(error ? "save failed" : (data?.message ?? "saved"));
  };

  return (
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
        placeholder={"# .env — encrypted at rest, synced to every checkout\nAPI_KEY=…"}
        value={value}
        onChange={(e) => setContent(e.target.value)}
        spellCheck={false}
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
        <button className="btn primary small" onClick={save} disabled={busy}>
          {busy ? "saving…" : "save & sync"}
        </button>
        {status && <span className="muted small">{status}</span>}
        <span className="faint small" style={{ marginLeft: "auto" }}>
          AES-256-GCM · pushed to online checkouts on save & on clone
        </span>
      </div>
    </div>
  );
}

export function WorkspaceDetail() {
  const { id } = useParams<{ id: string }>();
  const showNewWork = useNewWork((s) => s.show);
  const { data: ws } = useQuery({
    queryKey: ["workspaces", id],
    queryFn: async () =>
      (await api.GET("/api/v1/workspaces/{id}", { params: { path: { id: id! } } }))
        .data,
    enabled: !!id,
  });
  const { data: sessions } = useQuery({
    queryKey: ["sessions", id],
    queryFn: async () =>
      (
        await api.GET("/api/v1/sessions", {
          params: { query: { workspace_id: id } },
        })
      ).data ?? [],
    enabled: !!id,
  });

  if (!ws) return <Empty>Loading…</Empty>;

  return (
    <div
      className="nook-grid"
      style={{ gridTemplateColumns: "1.3fr 1fr", gridTemplateRows: "auto 1fr" }}
    >
      <Panel
        title={`Workspace · ${ws.name}`}
        actions={
          <button
            className="btn primary small"
            onClick={() => showNewWork({ workspaceId: ws.id })}
          >
            start work
          </button>
        }
      >
        <table className="nook-table">
          <thead>
            <tr>
              <th>Node</th>
              <th>Path</th>
              <th>Branch</th>
              <th>State</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {ws.locations.map((l) => (
              <tr key={`${l.node_id}:${l.path}`}>
                <td>
                  <StatusDot status={l.node_status} /> {l.node_name}
                </td>
                <td className="mono muted">{l.path}</td>
                <td className="mono">
                  {l.git_branch ?? "—"}{" "}
                  {l.worktree ? (
                    <Pill tone="info">worktree</Pill>
                  ) : (
                    <Pill tone="dim">primary</Pill>
                  )}
                </td>
                <td>
                  {l.dirty ? <Pill tone="warn">dirty</Pill> : <Pill tone="ok">clean</Pill>}
                </td>
                <td>
                  <button
                    className="btn small"
                    disabled={l.node_status !== "online"}
                    title="new worktree location"
                    onClick={() =>
                      showNewWork({
                        workspaceId: ws.id,
                        nodeId: l.node_id,
                        worktree: true,
                      })
                    }
                  >
                    + worktree
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </Panel>

      <Panel title="Sessions">
        {(sessions ?? []).length === 0 ? (
          <Empty>No sessions in this workspace yet.</Empty>
        ) : (
          <table className="nook-table">
            <tbody>
              {(sessions ?? []).map((s) => (
                <tr key={s.id}>
                  <td>
                    <Link className="bright" to={`/sessions/${s.id}`}>
                      {s.name}
                    </Link>
                  </td>
                  <td>
                    <Pill tone="accent">{s.runtime}</Pill>
                  </td>
                  <td>
                    <Pill tone={statusTone(s.status)}>{s.status}</Pill>
                  </td>
                  <td className="muted small">
                    {new Date(s.created_at).toLocaleString()}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Panel>

      <Panel title="env vault">
        <EnvPanel workspaceId={ws.id} />
      </Panel>

      <div
        className="nook-grid"
        style={{ gridTemplateRows: "1fr 1fr", gridTemplateColumns: "1fr" }}
      >
        <Panel title="Rolling notes">
          <NotesPanel workspaceId={ws.id} />
        </Panel>
        <Panel title="Activity">
          <ActivityFeed workspaceId={ws.id} limit={60} />
        </Panel>
      </div>
    </div>
  );
}

/** Delete a workspace, optionally removing its checkouts from disk.
 *  Records alone aren't enough: leave the files and discovery re-adds it. */
function DeleteWorkspaceButton({
  id,
  name,
  checkouts,
}: {
  id: string;
  name: string;
  checkouts: number;
}) {
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState(false);

  const del = async () => {
    let deleteFiles = false;
    if (checkouts > 0) {
      const answer = window.prompt(
        `Delete workspace "${name}"?\n\n` +
          `It has ${checkouts} checkout(s) on disk.\n\n` +
          `Type "files" to delete the checkouts too (destructive — the code is removed),\n` +
          `or "forget" to only remove it from NookOS (it will be rediscovered ` +
          `while the files remain).`,
        "forget",
      );
      if (answer === null) return;
      const choice = answer.trim().toLowerCase();
      if (choice !== "files" && choice !== "forget") return;
      deleteFiles = choice === "files";
    } else if (!window.confirm(`Delete workspace "${name}"?`)) {
      return;
    }

    setBusy(true);
    const { data, error, response } = await api.DELETE("/api/v1/workspaces/{id}", {
      params: { path: { id } },
      body: { delete_files: deleteFiles },
    });
    setBusy(false);
    if (error || !response.ok) {
      window.alert(
        response.status === 409
          ? "This workspace still has live sessions — kill them first."
          : `Delete failed: ${JSON.stringify(error)}`,
      );
      return;
    }
    queryClient.invalidateQueries();
    if (data?.checkouts_remaining) window.alert(data.message);
  };

  return (
    <button
      className="btn danger small icon"
      title="delete workspace"
      onClick={del}
      disabled={busy}
    >
      <Trash2 size={12} />
    </button>
  );
}
