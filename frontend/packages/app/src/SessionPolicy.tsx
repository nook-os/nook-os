// Declare a workspace's desired session state, and see whether it got it
// (MAIN-319 AC-1 + AC-3).
//
// The declaration is MAIN-315's `SessionSpec`; the numbers come from MAIN-316's
// planner through `/reconcile-status`, so what is on screen is what the loop is
// acting on rather than a second opinion about it.
import React, { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus, X } from "lucide-react";
import { api, type Schemas } from "@nookos/api";
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

/// A blocker, in the words a person can act on (MAIN-431: the reason is
/// structural now, so the pill can say WHY instead of just naming the node).
function blockerText(b: Schemas["NodeBlocker"]): string {
  switch (b.kind) {
    case "offline":
      return "offline";
    case "runtime_unavailable":
      return `no ${b.wanted} runtime (has ${b.available.join(", ") || "none"})`;
    case "selector_mismatch":
      return `${b.key}=${b.wanted} wanted, ${
        b.actual == null ? `no ${b.key} label` : `has ${b.actual}`
      }`;
    case "untolerated_taint":
      return `tainted ${b.key}:${b.effect}`;
    case "needs_clone":
      return "waiting on a clone";
  }
}

/// The status line, in the one sentence a person actually wants.
function StatusLine({
  status,
}: {
  status: Schemas["ReconcileStatus"];
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
          "broken" unless something says so — and until now saying so was ALL it
          did. `sessions.reconcile.enabled` had no CLI verb and no UI write
          anywhere: the only way to turn it on was a raw PUT to /settings, which
          made the whole declarative-sessions feature unreachable by anyone not
          hand-writing HTTP. The pill names the reason, so the pill is where the
          fix belongs. */}
      {!status.enabled && <ReconcileSwitch />}
      {status.shortfall > 0 && (
        <span className="faint small">
          {status.shortfall} short
          {status.blocked.length > 0 && (
            <>
              {" — "}
              {status.blocked
                .map(
                  (b) =>
                    `${b.node_name || b.node_id}: ${blockerText(b.reason)}`,
                )
                .join(", ")}
            </>
          )}
        </span>
      )}
    </span>
  );
}

/** Turn this tenant's reconciling on, from the thing that told you it was off.
 *
 *  Tenant-scoped, like the loops switch beside it in Settings — it governs the
 *  whole team's workspaces, not this one. Said plainly on the button, because a
 *  control that reads as workspace-local while writing tenant-wide state is how
 *  somebody turns on the fleet thinking they toggled one repo. */
function ReconcileSwitch() {
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState(false);

  const turnOn = async () => {
    setBusy(true);
    const { error } = await api.PUT("/api/v1/settings/{key}", {
      params: { path: { key: "sessions.reconcile.enabled" } },
      body: { scope: "tenant", value: true },
    });
    setBusy(false);
    if (error) {
      await notify(
        "Could not turn reconciling on",
        "The control plane refused the change.",
      );
      return;
    }
    queryClient.invalidateQueries();
  };

  return (
    <span style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
      <Pill tone="err" title="sessions.reconcile.enabled is off for this tenant">
        reconciling off
      </Pill>
      <button
        className="btn small"
        disabled={busy}
        title="turn reconciling on for this TEAM — every workspace in it starts converging"
        onClick={turnOn}
      >
        turn it on
      </button>
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
      <ReviewLoop workspaceId={workspaceId} />
    </Panel>
  );
}

/** The three states the API keeps distinct, in the words the CLI already prints.
 *
 *  Exported because this is the assertion that matters: `null` and `1` must not
 *  render the same string. Flattening them is what leaves somebody hunting for
 *  a switch nobody ever set, and it is the whole reason the column is nullable
 *  rather than defaulting to 1 in the schema. */
export function reviewLoopSummary(max: number | null): {
  state: string;
  detail: string;
} {
  if (max === null) {
    return { state: "unset (default 1)", detail: "the build's default ceiling" };
  }
  if (max === 0) {
    return { state: "0 (off)", detail: "this repo's PRs are not reviewed" };
  }
  return {
    state: `max ${max}`,
    // The ceiling is not yet a ceiling: nothing counts open PRs, so the number
    // IS what runs. The terminal says so already, and a surface that is quieter
    // than the terminal is how somebody concludes the number scales.
    detail: `no forge yet, so ${max} run regardless of open PRs`,
  };
}

type ReviewStatus = Schemas["ReviewLoopStatus"];

/** Which tenant switch is stopping this, named the way the person can fix it.
 *
 *  Reported in the order `pass()` checks them: reconciling is the outer gate,
 *  so naming loops first would send somebody to throw a switch that changes
 *  nothing while the real one stays off. */
export function reviewLoopGate(
  status: ReviewStatus | null | undefined,
): { key: string; what: string; where: string } | null {
  if (!status) return null;
  if (!status.reconcile_enabled) {
    return {
      key: "sessions.reconcile.enabled",
      what: "Reconciling is off for this team, so nothing converges.",
      where: "Settings → Sessions",
    };
  }
  if (!status.loops_enabled) {
    return {
      key: "loops.enabled",
      what: "Loops are off for this team, and a review loop is agent work.",
      where: "Settings → Loops",
    };
  }
  return null;
}

/** The server's own words for a refused write.
 *
 *  Its 400 names `max_replicas` — the field just typed into — so passing it
 *  through beats any sentence written here from a guess about which rule broke. */
function refusalText(error: unknown): string {
  const e = error as { error?: string } | undefined;
  return e?.error ?? JSON.stringify(error);
}

/** The per-repo review-loop ceiling, on the surface that holds the repo's other
 *  declarations (MAIN-447).
 *
 *  Its numbers come from `/review-loop-status`, which plans through the SAME
 *  `plan_now` the reconciler runs — not from counting sessions here. A count
 *  taken locally would be right today and wrong the moment MAIN-448's forge
 *  makes the desired number `min(open_prs, max)`, with nothing to notice it. */
export function ReviewLoop({ workspaceId }: { workspaceId: string }) {
  const queryClient = useQueryClient();
  const [draft, setDraft] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [refusal, setRefusal] = useState<string | null>(null);

  const { data: decl } = useQuery({
    queryKey: ["review-loop", workspaceId],
    queryFn: async () =>
      (
        await api.GET("/api/v1/workspaces/{id}/review-loop", {
          params: { path: { id: workspaceId } },
        })
      ).data ?? null,
  });
  const { data: status } = useQuery({
    queryKey: ["review-loop-status", workspaceId],
    queryFn: async () =>
      ((
        await api.GET("/api/v1/workspaces/{id}/review-loop-status", {
          params: { path: { id: workspaceId } },
        })
      ).data as ReviewStatus | undefined) ?? null,
    refetchInterval: 10000,
  });

  // `undefined` is "not fetched yet" and `null` is "no declaration" — kept
  // apart because collapsing them renders the loading state as "unset (default
  // 1)", which is a claim about the repo rather than about the request. A
  // workspace set to 0 would announce the opposite of its own setting until the
  // fetch landed.
  const loaded = decl !== undefined;
  const max = decl ? ((decl as { max_replicas?: number | null }).max_replicas ?? null) : null;
  const gate = reviewLoopGate(status);
  const summary = reviewLoopSummary(max);
  const editing = draft !== null;

  const save = async (next: number | null) => {
    setBusy(true);
    setRefusal(null);
    const { error } = await api.PUT("/api/v1/workspaces/{id}/review-loop", {
      params: { path: { id: workspaceId } },
      body: { max_replicas: next },
    });
    setBusy(false);
    if (error) {
      setRefusal(refusalText(error));
      return;
    }
    setDraft(null);
    queryClient.invalidateQueries({ queryKey: ["review-loop", workspaceId] });
    queryClient.invalidateQueries({ queryKey: ["review-loop-status", workspaceId] });
  };

  return (
    <div className="review-loop" data-testid="review-loop">
      <div className="small" style={{ display: "flex", alignItems: "center", gap: 8 }}>
        <b>Review loop</b>
        {status && (
          <span style={{ display: "inline-flex", alignItems: "center", gap: 8 }}>
            <Pill tone={status.shortfall > 0 ? "warn" : "ok"}>
              {status.running}/{status.desired} running
            </Pill>
            {status.shortfall > 0 && (
              <span className="faint small" data-testid="review-loop-shortfall">
                {status.shortfall} short
                {status.blocked.length > 0 && (
                  <>
                    {" — waiting on a clone to "}
                    {status.blocked.map((b) => b.node_name || b.node_id).join(", ")}
                  </>
                )}
                {status.blocked.length === 0 && status.port_capped && (
                  <> — this repo declares no ports, so it is held to one per node</>
                )}
              </span>
            )}
          </span>
        )}
      </div>

      {!loaded ? (
        <div className="policy-summary small mono faint">reading the declaration…</div>
      ) : !editing ? (
        <div className="policy-summary small mono">
          <div data-testid="review-loop-state">{summary.state}</div>
          <div className="faint">{summary.detail}</div>
        </div>
      ) : (
        <div style={{ display: "flex", alignItems: "center", gap: 6, marginTop: 4 }}>
          <input
            className="input small"
            type="number"
            min={0}
            aria-label="review loop maximum"
            value={draft}
            disabled={busy}
            onChange={(e) => setDraft(e.target.value)}
          />
          <button
            className="btn primary small"
            disabled={busy}
            onClick={() => save(draft.trim() === "" ? null : Number(draft))}
          >
            save
          </button>
          <button className="btn small" disabled={busy} onClick={() => setDraft(null)}>
            cancel
          </button>
        </div>
      )}

      {refusal && (
        <div className="small" data-testid="review-loop-refusal" style={{ color: "var(--err)" }}>
          {refusal}
        </div>
      )}

      {/* Disabled and explained, rather than hidden: a control that vanishes
          teaches nothing about why, which is the dead end MAIN-297 named. */}
      {gate && (
        <div className="faint small" data-testid="review-loop-gate">
          {gate.what} Turn it on in {gate.where}.
        </div>
      )}

      {!editing && (
        <div style={{ display: "flex", gap: 6, marginTop: 4 }}>
          <button
            className="btn small"
            disabled={busy || !!gate}
            title={gate ? gate.what : undefined}
            onClick={() => setDraft(max === null ? "" : String(max))}
          >
            set a maximum
          </button>
          {/* Clearing has to be reachable, and it is not the same as typing 1
              (AC-2) — this is the only control that can put the column back to
              the state a fresh workspace is in. */}
          {max !== null && (
            <button
              className="btn small"
              disabled={busy || !!gate}
              title="back to unset — the build's default ceiling"
              onClick={() => save(null)}
            >
              clear
            </button>
          )}
        </div>
      )}
    </div>
  );
}
