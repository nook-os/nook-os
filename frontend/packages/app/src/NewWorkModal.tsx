// The one place to start work — two tabs matched to how you actually think:
//   • New       → type a git URL (clones) or a new name (creates a project).
//   • Existing  → PICK a workspace from a filterable list (no typing exact
//                 names, so no typos).
// Then optionally tick "new worktree branch", pick node (Auto by default) and
// runtime. Workspace = repo = project (one word).
import React, { useEffect, useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "react-router-dom";
import { X } from "lucide-react";
import { api, type NodeInfo } from "@nookos/api";
import { defaultRuntime, RuntimePicker } from "@nookos/ui";
import { useNewWork } from "./newwork";
import { PORT_CAP_SENTENCE } from "./PortSafetyNotice";
import { onJobFinish, useJobs } from "./jobs";
import { WorkspaceLocations } from "./WorkspaceLocations";
import { adoptEnvFromDisk, saveEnv } from "./envvault";
import { requireAppPassword } from "./apppassword";

const AUTO = "";
type Tab = "new" | "existing";

const looksLikeGitUrl = (q: string) =>
  /^(https?:\/\/|git@|ssh:\/\/|git:\/\/)/.test(q.trim()) || /\.git$/.test(q.trim());

/** "git@github.com:acme/services.git" → "acme/services" */
function repoLabel(url: string): string {
  const tail = url.trim().replace(/\.git$/, "").replace(/\/$/, "");
  const parts = tail.split(/[/:]/).filter(Boolean);
  return parts.slice(-2).join("/") || tail;
}

/** Rendered once (in Shell); opens via the useNewWork store. */
export function NewWorkHost() {
  const open = useNewWork((s) => s.open);
  return open ? <NewWorkModal /> : null;
}

function NewWorkModal() {
  const { seed, hide } = useNewWork();
  const navigate = useNavigate();
  const queryClient = useQueryClient();

  const taskId = seed.taskId;
  // Seed-driven, not user-switchable: the tab bar is gone. A workspace-scoped
  // open (add-worktree, task) shows the existing view; everything else deploys.
  const [tab] = useState<Tab>(seed.workspaceId ? "existing" : "new");
  const [query, setQuery] = useState(""); // new-tab input (URL or name)
  const [filter, setFilter] = useState(""); // existing-tab filter
  const [selectedId, setSelectedId] = useState<string | null>(seed.workspaceId ?? null);
  const [credentialId, setCredentialId] = useState("");
  const [worktree, setWorktree] = useState(seed.worktree ?? false);
  const [branch, setBranch] = useState("");
  const [nodeId, setNodeId] = useState(seed.nodeId ?? AUTO);
  // Empty until the chosen node reports what it has; the effect below picks.
  const [runtime, setRuntime] = useState("");
  const [envText, setEnvText] = useState("");
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<string | null>(null);

  const useWorktree = worktree || !!taskId;

  const { data: nodes } = useQuery({
    queryKey: ["nodes"],
    queryFn: async () => (await api.GET("/api/v1/nodes")).data ?? [],
  });
  // Own vs. teammate's: the server refuses a session start on a node you don't
  // own (MAIN-130), so a non-owned node is never a spawn target. Same `["me"]`
  // key the rest of the app uses — this rides the existing fetch (MAIN-132).
  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
  const { data: workspaces } = useQuery({
    queryKey: ["workspaces"],
    queryFn: async () => (await api.GET("/api/v1/workspaces")).data ?? [],
  });
  const { data: credentials } = useQuery({
    queryKey: ["git-credentials"],
    queryFn: async () => (await api.GET("/api/v1/git-credentials")).data ?? [],
  });

  // Starting work here IS a spawn, so offer nodes the caller may run on: their
  // own, OR a node its owner has shared with the team (MAIN-136, widening the
  // MAIN-130 owner-only rule). A teammate's *unshared* node stays refused
  // server-side, so it is not offered here.
  const online = (nodes ?? []).filter(
    (n) =>
      n.status === "online" &&
      (n.owner_person_id === me?.person_id || n.shared),
  );
  const q = query.trim();

  // New-tab intent: URL → clone, otherwise → new project.
  const newIntent: "clone" | "project" | null = !q
    ? null
    : looksLikeGitUrl(q)
      ? "clone"
      : "project";

  const selectedWorkspace = (workspaces ?? []).find((w) => w.id === selectedId);
  const boundWorkspaceId = tab === "existing" ? selectedWorkspace?.id ?? "" : "";

  const filtered = (workspaces ?? []).filter(
    (w) => !filter.trim() || w.name.toLowerCase().includes(filter.trim().toLowerCase()),
  );

  const eligibleNodes: NodeInfo[] = useMemo(() => {
    if (boundWorkspaceId) {
      const ws = (workspaces ?? []).find((w) => w.id === boundWorkspaceId);
      const ids = new Set(
        (ws?.locations ?? [])
          .filter((l) => l.node_status === "online")
          .map((l) => l.node_id),
      );
      return online.filter((n) => ids.has(n.id));
    }
    return online;
  }, [boundWorkspaceId, workspaces, online]);

  const { data: autoPick } = useQuery({
    queryKey: ["schedule-node", boundWorkspaceId || "any"],
    enabled: nodeId === AUTO,
    queryFn: async () =>
      (
        await api.GET("/api/v1/schedule/node", {
          params: { query: { workspace_id: boundWorkspaceId || undefined } },
        })
      ).data ?? null,
    retry: false,
    refetchInterval: 10000,
  });

  const effectiveNode: NodeInfo | undefined =
    nodeId === AUTO
      ? (nodes ?? []).find((n) => n.id === autoPick?.node_id)
      : (nodes ?? []).find((n) => n.id === nodeId);

  const runtimes =
    ((effectiveNode?.capabilities as Record<string, unknown>)?.runtimes as string[]) ??
    ["bash"];

  // Declarative "New Workspace": you don't pick a node, you declare intent and
  // the reconciler places it. The runtime choices are therefore the UNION of
  // what the whole fleet can launch, not one node's — a workspace asking for
  // `claude` will only land where claude exists (the reconciler's eligibility).
  const fleetRuntimes = useMemo(() => {
    const set = new Set<string>();
    for (const n of nodes ?? [])
      for (const r of ((n.capabilities as Record<string, unknown>)?.runtimes as
        | string[]
        | undefined) ?? [])
        set.add(r);
    return set.size ? [...set] : ["bash"];
  }, [nodes]);
  // "repo replicas": how many NODES hold a clone. Within each, every checkout
  // (clone + worktrees) gets a session — the per-worktree half of the model.
  const [replicaMode, setReplicaMode] = useState<"single" | "all" | "count">("all");
  const [replicaCount, setReplicaCount] = useState(2);
  const [selector, setSelector] = useState(""); // "key=val, k2=v2" (optional)

  // The declarative path IS the New tab (unless it's a task-start, which acts on
  // one node now). You give a repo and how to run it, and the fleet converges —
  // cloning, worktrees and placement all move server-side (the reconciler). The
  // fields show as soon as the tab opens, so the model is visible before you
  // type; the submit needs a real repo URL (canGo gates on it).
  const declarative = tab === "new" && !taskId;
  const pickerRuntimes = declarative ? fleetRuntimes : runtimes;

  // Starting work here means starting an agent on it: if the node has claude,
  // that's what the session opens with. (A plain shell is one click away in
  // the picker, and `defaultRuntime` still governs new terminals elsewhere,
  // where a shell is the right answer.)
  useEffect(() => {
    if (!pickerRuntimes.includes(runtime)) {
      setRuntime(
        pickerRuntimes.includes("claude") ? "claude" : defaultRuntime(pickerRuntimes),
      );
    }
  }, [pickerRuntimes.join(",")]);

  const resolveNode = async (): Promise<string> => {
    if (nodeId !== AUTO) return nodeId;
    if (autoPick?.node_id) return autoPick.node_id;
    const { data, error } = await api.GET("/api/v1/schedule/node", {
      params: { query: { workspace_id: boundWorkspaceId || undefined } },
    });
    if (error || !data) throw new Error("no online node available");
    return data.node_id;
  };

  /** Wait for discovery to surface a workspace. Cloned repos are named
   *  "owner/repo", so match the qualified name or its bare repo tail. */
  const pollWorkspace = async (name: string): Promise<string> => {
    const wanted = name.toLowerCase();
    const tail = (s: string) => s.toLowerCase().split("/").pop() ?? "";
    for (let i = 0; i < 20; i++) {
      const ws = (await api.GET("/api/v1/workspaces")).data ?? [];
      const match =
        ws.find((w) => w.name.toLowerCase() === wanted) ??
        ws.find((w) => tail(w.name) === tail(wanted));
      if (match) return match.id;
      await new Promise((r) => setTimeout(r, 700));
    }
    throw new Error("created, but discovery hasn't surfaced it yet — check Workspaces");
  };

  /** Start a background clone and hand control straight back. Returns the
   *  job id — there's no workspace to resolve yet, and waiting for one is
   *  exactly what we're avoiding. */
  const startBackgroundClone = async (node: string): Promise<string> => {
    const { data, error } = await api.POST("/api/v1/nodes/{id}/clone", {
      params: { path: { id: node } },
      body: { url: q, credential_id: credentialId || null, background: true },
    });
    if (error || !data?.ok) throw new Error(data?.message ?? "clone failed");
    if (data.path) {
      useJobs.getState().start({
        id: data.path,
        label: `Cloning ${repoLabel(q)}`,
        kind: "clone",
        href: "/admin?section=workspaces",
      });
    }
    return data.path ?? "";
  };

  const initAndResolve = async (node: string): Promise<string> => {
    const { data, error } = await api.POST("/api/v1/nodes/{id}/projects", {
      params: { path: { id: node } },
      body: { name: q },
    });
    if (error || !data?.ok) throw new Error(data?.message ?? "init failed");
    setStatus(data.message);
    return pollWorkspace(q);
  };

  const waitForLocation = async (ws: string, path: string) => {
    for (let i = 0; i < 20; i++) {
      const detail = (
        await api.GET("/api/v1/workspaces/{id}", { params: { path: { id: ws } } })
      ).data;
      if (detail?.locations.some((l) => l.path === path)) return;
      await new Promise((r) => setTimeout(r, 500));
    }
  };

  const startSession = async (ws: string, node: string, path?: string) => {
    if (path) await waitForLocation(ws, path);
    const { data, error } = await api.POST("/api/v1/sessions", {
      body: { workspace_id: ws, node_id: node, runtime, path: path ?? null },
    });
    if (error) throw new Error(JSON.stringify(error));
    queryClient.invalidateQueries();
    if (data) navigate(`/sessions/${data.id}`);
  };

  /** Parse "os=linux, gpu=yes" into a selector map; blank keys/values dropped. */
  const parseSelector = (): Record<string, string> => {
    const out: Record<string, string> = {};
    for (const pair of selector.split(",")) {
      const [k, v] = pair.split("=").map((s) => s.trim());
      if (k && v) out[k] = v;
    }
    return out;
  };

  const replicasSpec = () =>
    replicaMode === "count"
      ? { kind: "count", count: Math.max(0, replicaCount) }
      : { kind: replicaMode };

  /** The declarative path: register the repo and DECLARE how to run it. No node
   *  pick, no clone here — the reconciler clones onto eligible nodes and starts
   *  one session per checkout. Returns nothing; navigates to the workspace. */
  const createDeclarative = async () => {
    const { data: ws, error } = await api.POST("/api/v1/workspaces", {
      body: { name: repoLabel(q), git_remote_url: q.trim() },
    });
    if (error || !ws) throw new Error(JSON.stringify(error) || "could not create workspace");
    // Pin the key BEFORE the spec, because the spec is what the reconciler acts
    // on: turn reconcile on with no credential and clone-on-demand fires with
    // `ssh_key: None`, which no private repo authorizes (MAIN-367). The picker
    // used to be hidden on this path entirely, so a declaratively-added private
    // repo could never clone — the exact case that blocks a loop.
    if (credentialId) {
      const { error: credErr } = await api.PUT("/api/v1/workspaces/{id}/credential", {
        params: { path: { id: ws.id } },
        body: { credential_id: credentialId },
      });
      if (credErr) throw new Error(JSON.stringify(credErr));
    }
    const { error: specErr } = await api.PUT("/api/v1/workspaces/{id}/session-spec", {
      params: { path: { id: ws.id } },
      body: {
        spec: {
          runtime,
          node_selector: parseSelector(),
          tolerations: [],
          replicas: replicasSpec() as never,
        },
      },
    });
    if (specErr) throw new Error(JSON.stringify(specErr));
    queryClient.invalidateQueries();
    navigate(`/workspaces/${ws.id}`);
    hide();
  };

  const go = async () => {
    setBusy(true);
    setStatus(null);
    try {
      // Declarative "New Workspace" short-circuits the whole imperative flow.
      if (declarative) {
        await createDeclarative();
        return;
      }
      const node = await resolveNode();
      let ws = selectedWorkspace?.id ?? "";
      // Cloning can take minutes. Kick it off, let the HUD follow it, and
      // give the user their UI back — but "start work" still means start
      // work, so finish the job once the repo lands.
      if (tab === "new" && newIntent === "clone") {
        // Ask for the password now, while the user is still here. The clone
        // runs in the background and its .env save happens on completion.
        if (envText.trim() && !(await requireAppPassword())) {
          setBusy(false);
          setStatus("a .env needs your app password — nothing was started");
          return;
        }
        const jobId = await startBackgroundClone(node);
        const want = q.replace(/\/$/, "").replace(/\.git$/, "").split(/[/:]/).pop() ?? "";
        const wantWorktree = useWorktree;
        const wantBranch = branch.trim();
        const env = envText;
        if (jobId) {
          onJobFinish(jobId, async (ok) => {
            if (!ok) return;
            try {
              const ws = await pollWorkspace(want);
              // Sealed with the password captured before the clone started —
              // this callback fires minutes later, and a password prompt
              // arriving out of nowhere then is worse than useless.
              if (env.trim()) await saveEnv(ws, env);
              // The repo may have brought its own .env along.
              else await adoptEnvFromDisk(ws);
              let path: string | undefined;
              if (wantWorktree) {
                const { data } = await api.POST("/api/v1/workspaces/{id}/worktrees", {
                  params: { path: { id: ws } },
                  body: { node_id: node, branch: wantBranch || "work" },
                });
                path = data?.path ?? undefined;
              }
              const { data: session } = await api.POST("/api/v1/sessions", {
                body: { workspace_id: ws, node_id: node, runtime, path: path ?? null },
              });
              queryClient.invalidateQueries();
              // Don't yank focus minutes later — make it one click instead.
              if (session) {
                useJobs.getState().finish(
                  jobId,
                  true,
                  `ready — open ${session.name}`,
                  `/sessions/${session.id}`,
                );
              }
            } catch {
              // The repo landed even if the session didn't; the workspace is
              // there to start one from.
              queryClient.invalidateQueries();
            }
          });
        }
        hide();
        return;
      }
      if (tab === "new" && newIntent === "project") ws = await initAndResolve(node);

      // Pasted .env is sealed with the app password, then synced to every
      // online checkout, so the session starts with the app configured.
      if (envText.trim() && ws) {
        setStatus("sealing .env with your app password…");
        if (!(await saveEnv(ws, envText))) {
          setBusy(false);
          setStatus("a .env needs your app password — nothing was started");
          return;
        }
      } else if (ws) {
        // Existing workspace or fresh project: adopt a .env already on disk.
        await adoptEnvFromDisk(ws);
      }

      if (taskId) {
        const { data, error } = await api.POST("/api/v1/tasks/{id}/start-work", {
          params: { path: { id: taskId } },
          body: {
            node_id: node,
            runtime,
            branch: branch.trim() || null,
            workspace_id: ws || null,
          },
        });
        if (error) throw new Error(JSON.stringify(error));
        queryClient.invalidateQueries();
        if (data?.session) navigate(`/sessions/${data.session.id}`);
        hide();
        return;
      }

      if (useWorktree) {
        const { data, error } = await api.POST("/api/v1/workspaces/{id}/worktrees", {
          params: { path: { id: ws } },
          body: { node_id: node, branch: branch.trim() || "work" },
        });
        if (error || !data?.ok) throw new Error(data?.message ?? "worktree failed");
        await startSession(ws, node, data.path ?? undefined);
      } else {
        await startSession(ws, node);
      }
      hide();
    } catch (e) {
      setStatus(String(e instanceof Error ? e.message : e));
      setBusy(false);
    }
  };

  const canGo = declarative
    ? // Declarative deploy needs a real repo URL and nothing else — no online
      // node up front, the reconciler waits for one.
      looksLikeGitUrl(query)
    : (tab === "new" ? newIntent !== null : !!selectedWorkspace) &&
      // Every imperative path (task-start, ad-hoc on an existing workspace)
      // still needs a machine to act on now.
      (nodeId !== AUTO || !!autoPick?.node_id || online.length > 0);

  return (
    <div className="modal-backdrop" onMouseDown={hide}>
      <div className="modal" onMouseDown={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <span>{taskId ? "// start work on task" : "// new workspace"}</span>
          <button className="btn small" onClick={hide}>
            <X size={13} />
          </button>
        </div>

        <div className="modal-body">
          {/* No more "new vs existing" tabs: deploying a repo is declarative and
              the reconciler owns placement, so there is no hand-run "start a
              session on an existing workspace" anymore. The workspace-scoped
              opens (add-worktree from Mission, task start-work) still drive their
              own view via the seed; there is just no user-facing tab to switch. */}
          {tab === "new" ? (
            <>
              <div className="field">
                <label>
                  Repository URL
                  {newIntent === "clone" && <span className="intent-chip info">→ deploy</span>}
                  {query.trim() && newIntent !== "clone" && (
                    <span className="intent-chip">enter a git URL</span>
                  )}
                </label>
                <input
                  className="input mono"
                  placeholder="https://github.com/org/repo · git@github.com:org/repo.git"
                  value={query}
                  onChange={(e) => setQuery(e.target.value)}
                  autoFocus
                />
              </div>
              {/* Said BEFORE they finish (MAIN-361 AC-5), not discovered later
                  as an unexplained refusal to start a second session. The
                  sentence is shared with the workspace banner so the two cannot
                  drift into describing the same rule differently. */}
              {query.trim() !== "" && (
                <div className="faint small" style={{ marginTop: -4, marginBottom: 6 }}>
                  {PORT_CAP_SENTENCE} Add a{" "}
                  <span className="mono">.nook.toml</span> to the repo (or declare
                  its ports on the workspace afterwards) and the limit lifts.
                </div>
              )}
              {newIntent === "clone" && (
                <div className="field">
                  <label>SSH credential (for private repos)</label>
                  <select className="input" value={credentialId} onChange={(e) => setCredentialId(e.target.value)}>
                    <option value="">node's own key</option>
                    {(credentials ?? []).map((c) => (
                      <option key={c.id} value={c.id}>🔑 {c.name}</option>
                    ))}
                  </select>
                </div>
              )}
            </>
          ) : (
            <div className="field">
              <label>Pick a workspace</label>
              <input
                className="input"
                placeholder="filter…"
                value={filter}
                onChange={(e) => setFilter(e.target.value)}
                autoFocus
              />
              <div className="suggest-list" style={{ maxHeight: 220 }}>
                {filtered.length === 0 && (
                  <div className="empty" style={{ height: "auto", padding: 14 }}>
                    no workspace matches
                  </div>
                )}
                {filtered.map((w) => (
                  <button
                    key={w.id}
                    className={`suggest-item${selectedId === w.id ? " active" : ""}`}
                    onClick={() => setSelectedId(w.id)}
                  >
                    <span className="bright">{w.name}</span>
                    <WorkspaceLocations locations={w.locations} />
                  </button>
                ))}
              </div>
            </div>
          )}

          {!taskId && !declarative && (
            <label className="check-row">
              <input
                type="checkbox"
                checked={worktree}
                onChange={(e) => setWorktree(e.target.checked)}
              />
              Isolate on a new worktree branch
              <span className="faint small">
                (a separate checkout/branch — safe to delete when done)
              </span>
            </label>
          )}

          {useWorktree && (
            <div className="field">
              <label>Branch{taskId ? " (defaults to task slug)" : ""}</label>
              <input
                className="input mono"
                placeholder="feature/thing"
                value={branch}
                onChange={(e) => setBranch(e.target.value)}
              />
            </div>
          )}

          {/* Declarative mode picks the node(s) for you — you declare intent,
              the reconciler places it. The imperative paths (task-start, ad-hoc
              on an existing workspace) still pick a machine. */}
          {!declarative && (
            <div className="field">
              <label>
                Where (node)
                {nodeId === AUTO && effectiveNode && (
                  <span className="faint"> · Auto → {effectiveNode.name}</span>
                )}
              </label>
              {(nodes ?? []).length === 0 ? (
                // No machine to run on — browse-only, so explain rather than
                // showing an empty picker that can't start anything (MAIN-132).
                <div className="empty" style={{ height: "auto", padding: 14 }}>
                  No machines yet — run <span className="mono">nook join</span> on a
                  computer to add one.
                </div>
              ) : (
                <select className="input" value={nodeId} onChange={(e) => setNodeId(e.target.value)}>
                  <option value={AUTO}>Auto (best available)</option>
                  {eligibleNodes.map((n) => {
                    // A shared node belongs to a teammate — label it so nobody
                    // mistakes it for their own machine (MAIN-136 AC-3).
                    const sharedTag =
                      n.shared && n.owner_person_id !== me?.person_id ? " · shared" : "";
                    return (
                      <option key={n.id} value={n.id}>
                        {n.name} · {n.platform}
                        {sharedTag}
                      </option>
                    );
                  })}
                </select>
              )}
            </div>
          )}

          <div className="field">
            <label>Runtime — a session runs an AI agent or a shell, your pick</label>
            <RuntimePicker available={pickerRuntimes} value={runtime} onChange={setRuntime} />
          </div>

          {declarative && (
            <>
              <div className="field">
                <label>
                  Replicas — how many machines hold a clone of the repo{" "}
                  <span className="faint">
                    (each also gets a session per worktree)
                  </span>
                </label>
                <div className="mode-tabs" style={{ marginTop: 2 }}>
                  <button
                    className={`mode-tab${replicaMode === "single" ? " active" : ""}`}
                    onClick={() => setReplicaMode("single")}
                  >
                    One node
                  </button>
                  <button
                    className={`mode-tab${replicaMode === "all" ? " active" : ""}`}
                    onClick={() => setReplicaMode("all")}
                  >
                    Every eligible node
                  </button>
                  <button
                    className={`mode-tab${replicaMode === "count" ? " active" : ""}`}
                    onClick={() => setReplicaMode("count")}
                  >
                    A set number
                  </button>
                </div>
                {replicaMode === "count" && (
                  <input
                    className="input mono"
                    style={{ marginTop: 6, maxWidth: 120 }}
                    type="number"
                    min={0}
                    value={replicaCount}
                    onChange={(e) => setReplicaCount(Number(e.target.value))}
                  />
                )}
              </div>

              <div className="field">
                <label>
                  Node selector{" "}
                  <span className="faint">
                    (optional — e.g. <span className="mono">os=linux, gpu=yes</span>; blank = anywhere)
                  </span>
                </label>
                <input
                  className="input mono"
                  placeholder="os=linux, gpu=yes"
                  value={selector}
                  onChange={(e) => setSelector(e.target.value)}
                />
              </div>
            </>
          )}

          <div className="field">
            <label>
              .env — paste it and the agent starts ready to run{" "}
              <span className="faint">
                (encrypted in the vault, synced to every checkout)
              </span>
            </label>
            <textarea
              className="input env-paste"
              rows={envText ? 8 : 3}
              spellCheck={false}
              placeholder={"DATABASE_URL=postgres://…\nAPI_KEY=…"}
              value={envText}
              onChange={(e) => setEnvText(e.target.value)}
            />
            {envText.trim() && (
              <div className="faint small" style={{ marginTop: 4 }}>
                {envText.trim().split("\n").filter((l) => l.trim() && !l.trim().startsWith("#")).length}{" "}
                variable(s) · saved as <span className="mono">.env</span>
              </div>
            )}
          </div>
        </div>

        <div className="modal-footer">
          <button className="btn primary" onClick={go} disabled={!canGo || busy}>
            {busy
              ? "working…"
              : declarative
                ? "Create & deploy"
                : taskId
                  ? "start work"
                  : "start work"}
          </button>
          <button className="btn" onClick={hide} disabled={busy}>cancel</button>
          {status && <span className="muted small">{status}</span>}
          {declarative && !status && (
            <span className="muted small">
              the fleet clones it and starts sessions — no machine to pick
            </span>
          )}
        </div>
      </div>
    </div>
  );
}
