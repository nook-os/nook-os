import React, { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useParams } from "react-router-dom";
import { Eye, EyeOff, FolderGit2, Lock, Plus, Sparkles, Trash2 } from "lucide-react";
import { api, type WorkspaceDetail as WsDetail } from "@nookos/api";
import { Empty, PagedPanel, Panel, Pill, RowAction, RowActions, StatusDot, statusTone, type DataColumn } from "@nookos/ui";
import { ActivityFeed } from "./Activity";
import { NotesPanel } from "./Notes";
import { createSpecDraft } from "../newspec";
import { useNewWork } from "../newwork";
import { usePagedList } from "../paging";
import { fetchCredentials } from "../GitCredentials";
import { PortSafetyNotice } from "../PortSafetyNotice";
import { SessionPolicy } from "../SessionPolicy";
import { WorkspaceLocations } from "../WorkspaceLocations";
import { SectionedPage, type PageSection } from "../SectionedPage";
import { askChoice, askConfirm, askForm, askText, notify } from "../dialogs";
import { requireAppPassword, useAppPassword } from "../apppassword";
import { adoptEnvFromDisk, saveEnv } from "../envvault";
import { SessionOwner } from "../sessionOwner";

/**
 * Which stored ssh key this repo clones and fetches with (MAIN-367 AC-1).
 *
 * The pin is the whole ticket: `credential_id` used to live on a CLONE REQUEST,
 * used once and thrown away, so clone-on-demand had no key to send and every
 * node fell back to its own — which no private repo authorizes. Setting it here
 * is what makes "changeable afterwards" true; the picker in **+ New Workspace**
 * only covers the create half.
 *
 * Lists keys, never key material: the private half never leaves the control
 * plane except as transient material delivered to a node for one git command.
 */
function WorkspaceCredential({
  workspaceId,
  pinned,
}: {
  workspaceId: string;
  pinned: string | null;
}) {
  const queryClient = useQueryClient();
  const [saving, setSaving] = useState(false);
  const {
    data: creds,
    isLoading,
    isError,
  } = useQuery({ queryKey: ["git-credentials"], queryFn: fetchCredentials });

  const pick = async (id: string) => {
    setSaving(true);
    const { error } = await api.PUT("/api/v1/workspaces/{id}/credential", {
      params: { path: { id: workspaceId } },
      body: { credential_id: id || null },
    });
    setSaving(false);
    if (error) {
      await notify("Could not pin the key", JSON.stringify(error));
      return;
    }
    queryClient.invalidateQueries({ queryKey: ["workspaces", workspaceId] });
  };

  return (
    <Panel title="git credential">
      <div className="field">
        <label>Clone and fetch with</label>
        <select
          className="input"
          value={pinned ?? ""}
          disabled={saving || isLoading || isError}
          onChange={(e) => pick(e.target.value)}
        >
          <option value="">node's own key (public repos and local paths)</option>
          {(creds ?? []).map((c) => (
            <option key={c.id} value={c.id}>
              🔑 {c.name}
            </option>
          ))}
        </select>
      </div>
      {/* Disabled rather than merely empty while unknown: a picker offering only
          "node's own key" is a statement that no key is available, and choosing
          it would silently unpin whatever is already set. */}
      {isLoading ? (
        <div className="muted">Loading credentials…</div>
      ) : isError ? (
        <div className="muted">
          Could not load credentials — the list could not be read, which is not the
          same as there being none. Anything already pinned is unchanged.
        </div>
      ) : (creds ?? []).length === 0 ? (
        <div className="muted">
          No keys stored yet — add one under{" "}
          <Link to="/settings#git-credentials">Settings → Git credentials</Link>, then
          authorize its public half on the git host.
        </div>
      ) : null}
    </Panel>
  );
}

/**
 * One click from a repo to a Loop page with a seed box (MAIN-298).
 *
 * It lives here, on the workspace, because that is where a PM already is when
 * they have an idea about this repo — the alternative was knowing a task id and
 * typing a `/loop/` URL, which is the thing this card exists to delete.
 */
function NewSpecButton({ workspaceId, inRow = false }: { workspaceId: string; inRow?: boolean }) {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState(false);

  const go = async () => {
    setBusy(true);
    const ident = await createSpecDraft(workspaceId);
    setBusy(false);
    // `null` means it failed and the user has already seen why — stay put, so
    // the failure is not followed by a Loop page for a ticket that isn't there.
    if (!ident) return;
    // The board is now a card behind; it is the one place the draft shows up.
    queryClient.invalidateQueries({ queryKey: ["boards"] });
    navigate(`/loop/${ident}`);
  };

  if (inRow) {
    return (
      <RowAction
        icon={Sparkles}
        label="new spec"
        title="Draft a spec for this repo — files a backlog ticket and opens its Loop page"
        busy={busy}
        onClick={go}
      />
    );
  }
  return (
    <button
      className="btn small"
      title="Draft a spec for this repo — files a backlog ticket and opens its Loop page"
      onClick={go}
      disabled={busy}
    >
      <Sparkles size={12} /> {busy ? "…" : "new spec"}
    </button>
  );
}

/** The all-workspaces table, on the pagination contract: searched
 *  (name/slug/remote), sorted (name/created), cursor-walked. Back on the left
 *  rail as its own page — workspaces are what the app is ABOUT, and burying
 *  the table in Admin read as hiding the product from itself. */
export function WorkspacesPage() {
  const showNewWork = useNewWork((s) => s.show);
  const list = usePagedList({
    key: ["workspaces", "page"],
    fetch: async (params) =>
      (await api.GET("/api/v1/workspaces/page", { params: { query: params } })).data,
  });

  const columns: DataColumn<WsDetail>[] = [
    {
      key: "name",
      header: "Workspace",
      sortKey: "name",
      className: "ws-name-col",
      cell: (w) => (
        <Link className="bright" to={`/workspaces/${w.id}`}>
          {w.name}
        </Link>
      ),
    },
    {
      key: "where",
      header: "Where it lives",
      cell: (w) => <WorkspaceLocations locations={w.locations} />,
    },
    {
      key: "actions",
      header: "",
      cell: (w) => (
        <RowActions>
          <NewSpecButton workspaceId={w.id} inRow />
          <DeleteWorkspaceButton id={w.id} name={w.name} checkouts={w.locations.length} />
        </RowActions>
      ),
    },
  ];

  return (
    <div className="nook-grid cards">
      <PagedPanel
        title="Workspaces"
        list={list}
        columns={columns}
        rowKey={(w) => w.id}
        searchPlaceholder="Search name or remote…"
        searchLabel="Search workspaces"
        empty={
          <>
            No workspaces yet. Hit <b>+ New Workspace</b> to clone a repo or start a
            new project — or join a node and its repositories appear here.
          </>
        }
        actions={
          <button className="btn primary small" onClick={() => showNewWork()}>
            <Plus size={12} /> New Workspace
          </button>
        }
      />
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
  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
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
  const { data: nodes } = useQuery({
    queryKey: ["nodes"],
    queryFn: async () => (await api.GET("/api/v1/nodes")).data ?? [],
  });
  const queryClient = useQueryClient();

  // Clone this workspace's STORED remote onto another node — no URL to re-type
  // (MAIN-223 AC-2). The server authorizes the node (own/shared) and pins the new
  // checkout to this workspace id.
  const cloneToNode = async () => {
    if (!ws?.git_remote_url) {
      await notify(
        "No stored repo URL",
        "This workspace doesn't know its git remote yet. Clone it once with an explicit URL (+ New Workspace) and it will remember it.",
      );
      return;
    }
    const here = new Set(ws.locations.map((l) => l.node_id));
    const candidates = (nodes ?? []).filter((n) => n.status === "online");
    if (candidates.length === 0) {
      await notify("No online nodes", "Bring a node online to clone onto it.");
      return;
    }
    const nodeId = await askChoice({
      title: `Clone "${ws.name}" to another node`,
      description: ws.git_remote_url,
      choices: candidates.map((n) => ({
        value: n.id,
        label: n.name,
        description: here.has(n.id)
          ? "Already has a checkout — re-clone heals it in place."
          : undefined,
      })),
      confirmLabel: "clone",
    });
    if (!nodeId) return;
    const { error, response } = await api.POST("/api/v1/workspaces/{id}/clone", {
      params: { path: { id: id! } },
      body: { node_id: nodeId },
    });
    if (error || !response.ok) {
      await notify(
        "Clone failed",
        error ? String((error as { error: unknown }).error) : response.statusText,
      );
      return;
    }
    await queryClient.invalidateQueries({ queryKey: ["workspaces", id] });
    await notify("Clone requested", "The checkout will appear here once the node finishes.");
  };

  if (!ws) return <Empty>Loading…</Empty>;

  const sections: PageSection[] = [
    {
      id: "checkouts",
      title: "Checkouts",
      group: "Repo",
      keywords: ["nodes", "worktree", "branch", "clone", "locations", "dirty"],
      render: () => (
        <Panel title="Checkouts">
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
      ),
    },
    {
      id: "env",
      title: "Env vault",
      group: "Repo",
      keywords: ["secrets", ".env", "password", "encrypt", "sealed", "vault"],
      render: () => (
        <Panel title="env vault">
          <EnvPanel workspaceId={ws.id} />
        </Panel>
      ),
    },
    {
      id: "credential",
      title: "Git credential",
      group: "Repo",
      keywords: ["ssh", "key", "deploy key", "private repo", "clone", "credential"],
      render: () => <WorkspaceCredential workspaceId={ws.id} pinned={ws.git_credential_id ?? null} />,
    },
    {
      id: "sessions",
      title: "Sessions",
      group: "Work",
      keywords: ["terminals", "running", "agents"],
      render: () => (
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
                    <td>
                      <SessionOwner createdBy={s.created_by} meId={me?.user?.id} />
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
      ),
    },
    {
      id: "policy",
      title: "Session policy",
      group: "Work",
      keywords: ["reconcile", "replicas", "desired", "declarative", "ports", "cap"],
      // One flex column, not two grid tracks: the section shell gives every
      // child an equal-height row, and the cap banner (which usually renders
      // NOTHING) must not cost the policy half the screen when it appears.
      // The banner stays ABOVE the policy on purpose (MAIN-361 AC-6): the
      // policy's replica count is the number the cap is overriding.
      render: () => (
        <div style={{ display: "flex", flexDirection: "column", gap: 10, minHeight: 0 }}>
          {id && <PortSafetyNotice workspaceId={id} workspaceName={ws.name} />}
          {/* The grid wrapper hands the policy panel the column's remaining
              height, so this section fills the shell like every other one. */}
          <div style={{ flex: 1, minHeight: 0, display: "grid" }}>
            {id && <SessionPolicy workspaceId={id} />}
          </div>
        </div>
      ),
    },
    {
      id: "notes",
      title: "Rolling notes",
      group: "Journal",
      keywords: ["notes", "scratch", "handoff"],
      render: () => (
        <Panel title="Rolling notes">
          <NotesPanel workspaceId={ws.id} />
        </Panel>
      ),
    },
    {
      id: "activity",
      title: "Activity",
      group: "Journal",
      keywords: ["events", "history", "audit"],
      render: () => (
        <Panel title="Activity">
          <ActivityFeed workspaceId={ws.id} limit={60} />
        </Panel>
      ),
    },
  ];

  const intro = (
    <Panel
      title={`Workspace · ${ws.name}`}
      actions={
        <>
          <NewSpecButton workspaceId={ws.id} />
          <button
            className="btn small"
            title={
              ws.git_remote_url
                ? "Clone this workspace's stored remote onto another node"
                : "This workspace has no stored git remote URL yet"
            }
            onClick={cloneToNode}
          >
            clone to node…
          </button>
          <button
            className="btn primary small"
            onClick={() => showNewWork({ workspaceId: ws.id })}
          >
            start work
          </button>
        </>
      }
    >
      <div className="detail-id-head">
        <FolderGit2 size={14} />
        <WorkspaceLocations locations={ws.locations} />
        {ws.git_remote_url && (
          <span className="mono muted small" style={{ overflow: "hidden", textOverflow: "ellipsis" }}>
            {ws.git_remote_url}
          </span>
        )}
      </div>
    </Panel>
  );

  return <SectionedPage sections={sections} placeholder="find…" intro={intro} />;
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
    <RowAction icon={Trash2} danger title="delete this workspace" busy={busy} onClick={del} />
  );
}
