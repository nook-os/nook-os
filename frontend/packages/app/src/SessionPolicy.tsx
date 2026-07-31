// Declare a workspace's desired session state, and see whether it got it
// (MAIN-319 AC-1 + AC-3).
//
// The declaration is MAIN-315's `SessionSpec`; the numbers come from MAIN-316's
// planner through `/reconcile-status`, so what is on screen is what the loop is
// acting on rather than a second opinion about it.
import React, { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus, X } from "lucide-react";
import { api } from "@nookos/api";
import { Empty, Panel, Pill } from "@nookos/ui";
import { notify } from "./dialogs";

type Replicas =
  | { kind: "count"; count: number }
  | { kind: "single" }
  | { kind: "all" };

interface Spec {
  runtime: string;
  node_selector: Record<string, string>;
  tolerations: { key: string; effect: string }[];
  replicas: Replicas;
}

const EMPTY: Spec = {
  runtime: "claude",
  node_selector: {},
  tolerations: [],
  replicas: { kind: "single" },
};

/** Key/value rows, kept as a LIST rather than an object while editing.
 *
 *  An object cannot hold a half-typed key: the moment you clear one to retype
 *  it, its value is orphaned onto `""` and the row you were editing merges with
 *  any other blank one. The list is converted to an object only on save. */
type Pair = { k: string; v: string };

function pairsOf(o: Record<string, string>): Pair[] {
  return Object.entries(o).map(([k, v]) => ({ k, v }));
}

/// The status line, in the one sentence a person actually wants.
function StatusLine({
  status,
}: {
  status: {
    enabled: boolean;
    managed: boolean;
    desired: number;
    running: number;
    shortfall: number;
    eligible: number;
    blocked: { node_id: string; node_name: string; reason: string }[];
  };
}) {
  if (!status.managed) {
    return (
      <span className="faint small">
        Unmanaged — sessions here are whatever you start by hand.
      </span>
    );
  }
  return (
    <span style={{ display: "inline-flex", alignItems: "center", gap: 8 }}>
      <Pill tone={status.shortfall > 0 ? "warn" : "ok"}>
        {status.running}/{status.desired} running
      </Pill>
      {/* A spec with the switch off converges never, which looks exactly like
          "broken" unless something says so. */}
      {!status.enabled && (
        <Pill tone="err" title="sessions.reconcile.enabled is off for this tenant">
          reconciling off
        </Pill>
      )}
      {status.shortfall > 0 && (
        <span className="faint small">
          {status.shortfall} short
          {status.blocked.length > 0 && (
            <>
              {" — waiting on a clone to "}
              {status.blocked.map((b) => b.node_name || b.node_id).join(", ")}
            </>
          )}
        </span>
      )}
    </span>
  );
}

export function SessionPolicy({ workspaceId }: { workspaceId: string }) {
  const queryClient = useQueryClient();
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState<Spec>(EMPTY);
  const [selector, setSelector] = useState<Pair[]>([]);
  const [tolerations, setTolerations] = useState<Pair[]>([]);
  const [busy, setBusy] = useState(false);

  const { data: spec } = useQuery({
    queryKey: ["session-spec", workspaceId],
    queryFn: async () =>
      (
        await api.GET("/api/v1/workspaces/{id}/session-spec", {
          params: { path: { id: workspaceId } },
        })
      ).data ?? null,
  });
  const { data: status } = useQuery({
    queryKey: ["reconcile-status", workspaceId],
    queryFn: async () =>
      (
        await api.GET("/api/v1/workspaces/{id}/reconcile-status", {
          params: { path: { id: workspaceId } },
        })
      ).data ?? null,
    // The loop converges in the background; without this the panel would show
    // "1 short" long after it stopped being true.
    refetchInterval: 10000,
  });

  // Seed the editor from what is stored, each time it opens.
  useEffect(() => {
    if (!editing) return;
    const s = (spec as Spec | null) ?? EMPTY;
    setDraft(s);
    setSelector(pairsOf(s.node_selector ?? {}));
    setTolerations((s.tolerations ?? []).map((t) => ({ k: t.key, v: t.effect })));
  }, [editing, spec]);

  const save = async (next: Spec | null) => {
    setBusy(true);
    const { error } = await api.PUT("/api/v1/workspaces/{id}/session-spec", {
      params: { path: { id: workspaceId } },
      body: { spec: next },
    });
    setBusy(false);
    if (error) {
      // The server refuses a spec that cannot mean anything — an empty runtime,
      // a blank selector key, an unknown taint effect. Its words, not ours.
      await notify("The control plane refused that policy", JSON.stringify(error));
      return;
    }
    setEditing(false);
    queryClient.invalidateQueries({ queryKey: ["session-spec", workspaceId] });
    queryClient.invalidateQueries({ queryKey: ["reconcile-status", workspaceId] });
  };

  const submit = () =>
    save({
      ...draft,
      // Blank rows are dropped rather than sent: a half-typed key is a UI state,
      // not a declaration, and the server would (correctly) reject it.
      node_selector: Object.fromEntries(
        selector.filter((p) => p.k.trim() && p.v.trim()).map((p) => [p.k.trim(), p.v.trim()]),
      ),
      tolerations: tolerations
        .filter((p) => p.k.trim())
        .map((p) => ({ key: p.k.trim(), effect: p.v.trim() || "NoSchedule" })),
    });

  const pairEditor = (
    rows: Pair[],
    set: (r: Pair[]) => void,
    kPlaceholder: string,
    vPlaceholder: string,
  ) => (
    <>
      {rows.map((p, i) => (
        <div key={i} className="policy-pair">
          <input
            className="input small"
            placeholder={kPlaceholder}
            value={p.k}
            onChange={(e) =>
              set(rows.map((r, j) => (i === j ? { ...r, k: e.target.value } : r)))
            }
          />
          <input
            className="input small"
            placeholder={vPlaceholder}
            value={p.v}
            onChange={(e) =>
              set(rows.map((r, j) => (i === j ? { ...r, v: e.target.value } : r)))
            }
          />
          <button
            className="btn small icon"
            title="remove"
            onClick={() => set(rows.filter((_, j) => j !== i))}
          >
            <X size={11} />
          </button>
        </div>
      ))}
      <button className="btn small" onClick={() => set([...rows, { k: "", v: "" }])}>
        <Plus size={11} /> add
      </button>
    </>
  );

  return (
    <Panel
      title="Session policy"
      actions={
        <span style={{ display: "inline-flex", alignItems: "center", gap: 8 }}>
          {status && <StatusLine status={status} />}
          {!editing && (
            <button className="btn small" onClick={() => setEditing(true)}>
              {spec ? "edit" : "declare"}
            </button>
          )}
        </span>
      }
    >
      {!editing ? (
        spec ? (
          <div className="policy-summary small mono">
            <div>
              runtime <b>{(spec as Spec).runtime}</b> ·{" "}
              {(spec as Spec).replicas.kind === "count"
                ? `${((spec as Spec).replicas as { count: number }).count} replicas`
                : (spec as Spec).replicas.kind === "all"
                  ? "one per matching node"
                  : "exactly one"}
            </div>
            <div className="faint">
              {Object.keys((spec as Spec).node_selector ?? {}).length === 0
                ? "any node"
                : Object.entries((spec as Spec).node_selector)
                    .map(([k, v]) => `${k}=${v}`)
                    .join(", ")}
              {((spec as Spec).tolerations ?? []).length > 0 &&
                ` · tolerates ${(spec as Spec).tolerations
                  .map((t) => `${t.key}:${t.effect}`)
                  .join(", ")}`}
            </div>
          </div>
        ) : (
          <Empty>
            This workspace runs no declared sessions. Declare a policy and the
            control plane keeps them running for you.
          </Empty>
        )
      ) : (
        <div className="policy-editor">
          <label className="small">
            runtime
            <input
              className="input small"
              value={draft.runtime}
              onChange={(e) => setDraft({ ...draft, runtime: e.target.value })}
              placeholder="claude"
            />
          </label>

          <label className="small">
            replicas
            <select
              className="input small"
              value={draft.replicas.kind}
              onChange={(e) => {
                const kind = e.target.value as Replicas["kind"];
                setDraft({
                  ...draft,
                  replicas:
                    kind === "count" ? { kind: "count", count: 1 } : { kind },
                });
              }}
            >
              <option value="single">exactly one</option>
              <option value="count">a fixed number</option>
              <option value="all">one per matching node</option>
            </select>
          </label>
          {draft.replicas.kind === "count" && (
            <label className="small">
              how many
              <input
                className="input small"
                type="number"
                min={0}
                value={draft.replicas.count}
                onChange={(e) =>
                  setDraft({
                    ...draft,
                    replicas: {
                      kind: "count",
                      // 0 is legal and means "managed, wanting none" — a real
                      // declaration, distinct from having no policy at all.
                      count: Math.max(0, Number(e.target.value) || 0),
                    },
                  })
                }
              />
            </label>
          )}

          <div className="small">
            node selector <span className="faint">(empty matches every node)</span>
            {pairEditor(selector, setSelector, "os", "linux")}
          </div>

          <div className="small">
            tolerations <span className="faint">(taints this work accepts)</span>
            {pairEditor(tolerations, setTolerations, "key", "NoSchedule")}
          </div>

          <div className="policy-actions">
            <button className="btn primary small" disabled={busy} onClick={submit}>
              save policy
            </button>
            <button className="btn small" disabled={busy} onClick={() => setEditing(false)}>
              cancel
            </button>
            {spec && (
              <button
                className="btn danger small"
                disabled={busy}
                title="stop managing this workspace's sessions"
                onClick={() => save(null)}
              >
                clear
              </button>
            )}
          </div>
        </div>
      )}
    </Panel>
  );
}
