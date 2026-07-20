import React, { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useParams } from "react-router-dom";
import { Eye, EyeOff, Lock, Plus, Trash2 } from "lucide-react";
import { api } from "@nookos/api";
import { Empty, Panel, Pill, StatusDot, statusTone } from "@nookos/ui";
import { ActivityFeed } from "./Activity";
import { NotesPanel } from "./Notes";
import { useNewWork } from "../newwork";
import { WorkspaceLocations } from "../WorkspaceLocations";
import { askChoice, askConfirm, askForm, askText, notify } from "../dialogs";
import { requireAppPassword, useAppPassword } from "../apppassword";
import { adoptEnvFromDisk, saveEnv } from "../envvault";

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
  const [revealed, setRevealed] = useState(false);
  const held = useAppPassword((s) => s.passphrase);

  const { data: loaded, refetch } = useQuery({
    queryKey: ["secrets", workspaceId, ".env"],
    queryFn: async () => {
      const { data, response } = await api.GET(
        "/api/v1/workspaces/{id}/secrets/{name}",
        { params: { path: { id: workspaceId, name: ".env" } } },
      );
      if (response.status === 404)
        return { content: "", protected: false, ephemeral: false, exists: false };
      return {
        content: data?.content ?? "",
        protected: !!data?.protected,
        ephemeral: !!data?.ephemeral,
        exists: true,
      };
    },
    retry: false,
  });

  // A repo that arrived with its own .env: nothing in the vault yet, but a
  // file sitting in a checkout waiting to be adopted.
  const { data: onDisk, refetch: recheckDisk } = useQuery({
    queryKey: ["secrets", workspaceId, ".env", "on-disk"],
    queryFn: async () =>
      (
        await api.GET("/api/v1/workspaces/{id}/secrets/{name}/on-disk", {
          params: { path: { id: workspaceId, name: ".env" } },
        })
      ).data,
    retry: false,
  });
  const adoptable = !!onDisk?.found && !onDisk.in_vault;

  const isProtected = !!loaded?.protected;
  // Secrets stay hidden until deliberately revealed — a shoulder shouldn't be
  // enough to read them, and a sealed one genuinely isn't loaded yet.
  const hidden = !revealed;
  const value = content ?? loaded?.content ?? "";

  const adopt = async () => {
    setBusy(true);
    const ok = await adoptEnvFromDisk(workspaceId);
    setBusy(false);
    if (ok) {
      setStatus("imported · sealed & synced");
      refetch();
      recheckDisk();
    }
  };

  const reveal = async () => {
    // Every read goes through unlock now, sealed or not: a row that predates
    // sealing is re-sealed on the way past, so there's no second class of
    // secret that opens without the password.
    const passphrase = held ?? (await requireAppPassword());
    if (!passphrase) return;
    setBusy(true);
    const { data, error, response } = await api.POST(
      "/api/v1/workspaces/{id}/secrets/{name}/open",
      {
        params: { path: { id: workspaceId, name: ".env" } },
        body: { passphrase },
      },
    );
    setBusy(false);
    if (error || !response.ok) {
      await notify(
        response.status === 403 ? "Wrong app password" : "Unlock failed",
        response.status === 403
          ? "That password doesn't open this secret."
          : JSON.stringify(error),
      );
      return;
    }
    setContent(data?.content ?? "");
    setRevealed(true);
    setStatus("unlocked · synced to checkouts");
  };

  const save = async () => {
    let ephemeral = loaded?.ephemeral ?? false;
    if (!loaded?.exists) {
      ephemeral = await askConfirm({
        title: "Wipe from disk when sessions end?",
        description:
          "The encrypted copy stays in the vault; the file is removed from checkouts once no session is using the workspace.",
        confirmLabel: "yes, ephemeral",
      });
    }

    setBusy(true);
    // saveEnv is the only way a .env enters the vault: it asks for the app
    // password (setting one up first if there isn't one), seals, and syncs.
    const ok = await saveEnv(workspaceId, value, { ephemeral });
    setBusy(false);
    setStatus(ok ? "saved · sealed & synced" : "not saved");
    if (ok) {
      setRevealed(true);
      refetch();
    }
  };

  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%" }}>
      <div className="env-shell">
        <textarea
          className={`input mono small env-area${hidden ? " blurred" : ""}`}
          placeholder={"# .env — sealed with your app password\nAPI_KEY=…"}
          value={hidden && isProtected ? PLACEHOLDER : value}
          onChange={(e) => setContent(e.target.value)}
          spellCheck={false}
          readOnly={hidden}
        />
        {hidden && (
          <div className="env-veil">
            <Lock size={18} />
            <div className="env-veil-title">
              {adoptable && !loaded?.exists
                ? "this repo came with a .env"
                : ".env is sealed"}
            </div>
            <p className="muted small">
              {adoptable && !loaded?.exists
                ? `Found in ${onDisk?.checkout_path ?? "a checkout"}, outside the vault. Import it to encrypt it and carry it to your other machines.`
                : "Encrypted with your app password. NookOS cannot read it without you."}
            </p>
            {adoptable && !loaded?.exists ? (
              <button className="btn primary" onClick={adopt} disabled={busy}>
                <Lock size={13} /> encrypt & import
              </button>
            ) : (
              <button className="btn primary" onClick={reveal} disabled={busy}>
                <Eye size={13} /> unlock
              </button>
            )}
          </div>
        )}
      </div>
      <div
        style={{
          display: "flex",
          gap: 8,
          alignItems: "center",
          padding: 8,
          borderTop: "1px solid var(--nook-border)",
        }}
      >
        <button className="btn primary small" onClick={save} disabled={busy || hidden}>
          {busy ? "saving…" : "save & sync"}
        </button>
        {revealed && (
          <button
            className="btn small"
            onClick={() => {
              setRevealed(false);
              setContent(null);
            }}
          >
            <EyeOff size={12} /> hide
          </button>
        )}
        {isProtected && <Pill tone="ok">sealed</Pill>}
        {loaded?.ephemeral && <Pill tone="warn">ephemeral</Pill>}
        {status && <span className="muted small">{status}</span>}
        <span className="faint small" style={{ marginLeft: "auto" }}>
          AES-256-GCM · app password never leaves your browser
        </span>
      </div>
    </div>
  );
}

/** Shown behind the blur so the shape of a secret is suggested, not its text. */
const PLACEHOLDER = [
  "DATABASE_URL=postgres://xxxxxxxxxxxxxxxxxxxx",
  "API_KEY=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
  "STRIPE_SECRET=xxxxxxxxxxxxxxxxxxxxxxxx",
  "JWT_SECRET=xxxxxxxxxxxxxxxxxxxxxxxxxxxx",
].join("\n");

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
      const choice = await askChoice({
        title: `Delete workspace "${name}"`,
        description: `It has ${checkouts} checkout(s) on disk.`,
        choices: [
          {
            value: "forget",
            label: "Forget it — keep the code",
            description:
              "Removes it from NookOS only. Discovery will find the files again on the next scan.",
          },
          {
            value: "files",
            label: "Delete the checkouts too",
            description:
              "Destructive: the code is removed from every online node. Uncommitted work is lost.",
          },
        ],
        confirmLabel: "delete",
        danger: true,
      });
      if (!choice) return;
      deleteFiles = choice === "files";
    } else if (
      !(await askConfirm({
        title: `Delete workspace "${name}"`,
        description: "It has no checkouts on disk.",
        confirmLabel: "delete",
        danger: true,
      }))
    ) {
      return;
    }

    setBusy(true);
    const { data, error, response } = await api.DELETE("/api/v1/workspaces/{id}", {
      params: { path: { id } },
      body: { delete_files: deleteFiles },
    });
    setBusy(false);
    if (error || !response.ok) {
      await notify(
        "Delete failed",
        response.status === 409
          ? "This workspace still has live sessions — kill them first."
          : JSON.stringify(error),
      );
      return;
    }
    queryClient.invalidateQueries();
    if (data?.checkouts_remaining) await notify("Deleted", data.message);
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
