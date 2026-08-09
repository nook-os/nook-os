// What a workspace declares it binds, and what its sessions actually got
// (MAIN-360).
//
// MAIN-301 shipped the storage and `PUT /workspaces/{id}/ports`, and the only
// UI it added was the NODE's range and lease list. So the declaration that
// decides which variables a session gets was settable only by curl — while the
// node range, which an operator changes far less often, had a full editor.
//
// `.nook.toml` (MAIN-359) makes the committed file the normal path. This is the
// other half: seeing what is in force, and editing it for a repo you cannot
// commit to.
//
// The node's range and its per-node leases stay in `NodePorts.tsx` (NG-2). What
// is here is the workspace's own declaration, plus the leases its OWN sessions
// hold — the same numbers viewed from the other end, which is what makes "I
// declared API_PORT" and "this session got 4207" checkable in one place.
//
// The rationale that used to sit on every row now hangs off `title` instead.
// It is the same text; a control surface somebody opens daily should not make
// them re-read the design notes to find the field they came for.
import React, { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus, X } from "lucide-react";
import { api, type Schemas } from "@nookos/api";
import { Empty, Panel, Pill } from "@nookos/ui";

type Requirement = Schemas["PortRequirement"];

/** A fresh row. `required` defaults false for the same reason the server's
 *  default declaration does: most sessions are a shell or an agent, and a node
 *  out of ports should not stop somebody opening a terminal. */
const BLANK: Requirement = { name: "", env: "", protocol: "tcp", required: false };

/** Why `optional` is the setting whose consequence is invisible: the session
 *  starts either way and the variable is simply missing, so an app that falls
 *  back to a default falls back to the literal every other session uses too. */
const REQUIRED_HELP =
  "required: the session does not start unless this port is leased.\n" +
  "optional: the session still starts and the variable is unset — the app must " +
  "fail fast rather than use a default, because that default is the literal " +
  "every other session would fall back to as well.";

/** The two collisions the API and the merged migration already depend on, found
 *  BEFORE the save so the message lands on the offending field (AC-3).
 *
 *  Both fail quietly if they get through: a duplicate `name` breaks the lease
 *  table's `session_port_leases_one_per_name`, and a duplicate `env` means two
 *  listeners write one variable and one silently gets no port. The server
 *  refuses them too — this is not the enforcement, it is the part that can point
 *  at the row you typed. */
export function rowErrors(rows: Requirement[]): Record<number, { name?: string; env?: string }> {
  const out: Record<number, { name?: string; env?: string }> = {};
  const seenName = new Map<string, number>();
  const seenEnv = new Map<string, number>();
  rows.forEach((r, i) => {
    const name = r.name.trim();
    const env = r.env.trim();
    if (!name) out[i] = { ...out[i], name: "needs a name" };
    if (!env) out[i] = { ...out[i], env: "needs a variable" };
    // The node splices this straight into a session's environment, so a value
    // with a space or an `=` would be dropped or corrupt its neighbours.
    else if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(env))
      out[i] = { ...out[i], env: "letters, digits and _ only, not starting with a digit" };

    if (name) {
      const first = seenName.get(name);
      if (first !== undefined) out[i] = { ...out[i], name: `already used by row ${first + 1}` };
      else seenName.set(name, i);
    }
    if (env) {
      const first = seenEnv.get(env);
      if (first !== undefined) out[i] = { ...out[i], env: `already written by row ${first + 1}` };
      else seenEnv.set(env, i);
    }
  });
  return out;
}

export function WorkspacePorts({
  workspaceId,
  /** The workspace's RAW stored declaration, straight off the workspace row.
   *
   *  Not the same question as `/ports`, which answers "what will be leased" and
   *  therefore reports the default listener for a workspace that has declared
   *  nothing. Both are needed: absent here means undeclared, `[]` means declared
   *  none, and a UI that could not tell them apart would show the default as
   *  though somebody had chosen it (AC-1). */
  declaredRaw,
}: {
  workspaceId: string;
  declaredRaw?: unknown;
}) {
  const queryClient = useQueryClient();
  const [editing, setEditing] = useState(false);
  const [rows, setRows] = useState<Requirement[]>([]);
  const [busy, setBusy] = useState(false);

  // What WILL be leased — the declaration if there is one, else the default.
  const { data: effective } = useQuery<Requirement[]>({
    queryKey: ["workspace-ports", workspaceId],
    queryFn: async () =>
      ((
        await api.GET("/api/v1/workspaces/{id}/ports", {
          params: { path: { id: workspaceId } },
        })
      ).data as Requirement[] | undefined) ?? [],
  });

  // What was actually DECLARED. `undefined` is undeclared; an empty array is a
  // deliberate "this repo binds nothing".
  const declared: Requirement[] | undefined = Array.isArray(declaredRaw)
    ? (declaredRaw as Requirement[])
    : undefined;

  // The workspace's own sessions, for the lease list (AC-6). Scoped to this
  // workspace by the query rather than filtered here, so a big tenant does not
  // ship every session to draw one panel.
  const { data: sessions } = useQuery({
    queryKey: ["sessions", workspaceId, "ports"],
    queryFn: async () =>
      (
        await api.GET("/api/v1/sessions", {
          params: { query: { workspace_id: workspaceId, active: true } },
        })
      ).data ?? [],
    // Leases are handed out as sessions start; without this the panel would
    // show an empty list moments after somebody watched a session come up.
    refetchInterval: 10000,
  });

  useEffect(() => {
    if (!editing) return;
    // Editing starts from what is IN FORCE, so a workspace on the default sees
    // that default as its first row rather than an empty editor it has to guess
    // at — and saving it makes explicit what was implicit.
    setRows((declared ?? effective ?? []).map((r) => ({ ...r })));
  }, [editing, declared, effective]);

  const errors = rowErrors(rows);
  const invalid = Object.keys(errors).length > 0;

  const save = async () => {
    if (invalid) return;
    setBusy(true);
    const { error } = await api.PUT("/api/v1/workspaces/{id}/ports", {
      params: { path: { id: workspaceId } },
      body: {
        requirements: rows.map((r) => ({
          ...r,
          name: r.name.trim(),
          env: r.env.trim(),
        })),
      },
    });
    setBusy(false);
    if (error) return; // the global write-failure toast carries the server's message
    setEditing(false);
    queryClient.invalidateQueries({ queryKey: ["workspace-ports", workspaceId] });
    // BOTH queries, because this panel reads from both: `effective` is its own
    // query, but `declared` arrives as a prop off the workspace row. Refreshing
    // only the first leaves the panel rendering the pre-save declaration, and
    // the editor seeds from `declared ?? effective` — so the stale value wins
    // and a second edit silently round-trips the OLD list over the new one.
    queryClient.invalidateQueries({ queryKey: ["workspaces", workspaceId] });
  };

  const set = (i: number, patch: Partial<Requirement>) =>
    setRows((rs) => rs.map((r, n) => (n === i ? { ...r, ...patch } : r)));

  // Every live lease this workspace's sessions hold, flattened so the table is
  // one row per PORT rather than one per session — a session can hold several.
  const leases = (sessions ?? []).flatMap((s) =>
    (s.leased_ports ?? []).map((p) => ({ session: s.name, ...p })),
  );

  return (
    <Panel
      title="Ports"
      actions={
        editing ? (
          <>
            <button className="btn primary small" disabled={busy || invalid} onClick={save}>
              save
            </button>
            <button className="btn small" disabled={busy} onClick={() => setEditing(false)}>
              cancel
            </button>
          </>
        ) : (
          // Not gated further than the server gates it (AC-5): changing a
          // declaration is `require_user`, exactly as changing the session spec
          // beside it is. A node credential cannot reach this UI at all.
          <button className="btn small" onClick={() => setEditing(true)}>
            edit
          </button>
        )
      }
    >
      <div className="ports-body">
        <div
          className="faint small"
          title="An app binds the variable, never a literal — which is what lets two worktrees of one repo run side by side instead of fighting over 3000."
        >
          One leased port per listener, delivered as the variable named.
        </div>

        {editing ? (
          <div className="ports-editor">
            <div className="ports-grid ports-head faint small">
              <span>name</span>
              <span>variable</span>
              <span>proto</span>
              <span title={REQUIRED_HELP}>required</span>
              <span />
            </div>

            {rows.map((r, i) => (
              <React.Fragment key={i}>
                <div className="ports-grid ports-row">
                  <input
                    className="input small"
                    placeholder="web"
                    aria-label={`listener ${i + 1} name`}
                    value={r.name}
                    onChange={(e) => set(i, { name: e.target.value })}
                  />
                  <input
                    className="input small mono"
                    placeholder="PORT"
                    aria-label={`listener ${i + 1} variable`}
                    value={r.env}
                    onChange={(e) => set(i, { env: e.target.value })}
                  />
                  <select
                    className="input small"
                    aria-label={`listener ${i + 1} protocol`}
                    value={r.protocol ?? "tcp"}
                    onChange={(e) => set(i, { protocol: e.target.value })}
                  >
                    <option value="tcp">tcp</option>
                    <option value="udp">udp</option>
                  </select>
                  <input
                    type="checkbox"
                    aria-label={`listener ${i + 1} required`}
                    title={REQUIRED_HELP}
                    checked={!!r.required}
                    onChange={(e) => set(i, { required: e.target.checked })}
                  />
                  <button
                    className="btn small icon"
                    aria-label={`remove listener ${i + 1}`}
                    onClick={() => setRows((rs) => rs.filter((_, n) => n !== i))}
                  >
                    <X size={11} />
                  </button>
                </div>
                {(errors[i]?.name || errors[i]?.env) && (
                  <div className="ports-err small">
                    {errors[i]?.name && <span>name: {errors[i]?.name}. </span>}
                    {errors[i]?.env && <span>variable: {errors[i]?.env}.</span>}
                  </div>
                )}
              </React.Fragment>
            ))}

            <div className="ports-actions">
              <button className="btn small" onClick={() => setRows((rs) => [...rs, { ...BLANK }])}>
                <Plus size={11} /> listener
              </button>
              {/* Saving an empty list is a real statement, not a no-op, so it
                  is worth saying out loud before somebody removes the last row
                  and wonders whether anything happened. */}
              {rows.length === 0 && (
                <span className="faint small">saving none declares that this repo binds nothing</span>
              )}
            </div>

            {/* MAIN-360 AC-4, as far as it can honestly go today. The warning
                is UNCONDITIONAL because nothing records where a stored
                declaration came from: `.nook.toml` (MAIN-359) writes the same
                field the API writes, with no provenance beside it. Warning
                always is the safe direction — it can be ignored by a repo that
                commits no file, whereas staying silent would let somebody save
                an edit that a scan quietly reverts. Making it conditional needs
                a `source` on the declaration; noted on the card rather than
                invented here. */}
            <div className="ports-note faint small">
              If this repo commits a <span className="mono">.nook.toml</span>, the next scan
              replaces what you save here — edit the file instead.
            </div>
          </div>
        ) : declared === undefined ? (
          <Empty>
            Declares nothing yet — so sessions here get the default:{" "}
            <span className="mono">
              {(effective ?? []).map((r) => r.env).join(", ") || "no ports"}
            </span>
            .
          </Empty>
        ) : declared.length === 0 ? (
          // An empty declaration is not the same as no declaration, and a blank
          // panel would read as a loading state (AC-1).
          <Empty>Declares no ports — sessions here start with no port variables.</Empty>
        ) : (
          <div className="ports-list">
            {declared.map((r) => (
              <div key={r.name} className="ports-listrow">
                <span className="mono">{r.name}</span>
                <span className="mono faint">{r.env}</span>
                <Pill>{r.protocol ?? "tcp"}</Pill>
                {r.required ? (
                  <Pill tone="warn" title={REQUIRED_HELP}>
                    required
                  </Pill>
                ) : (
                  <span className="faint small" title={REQUIRED_HELP}>
                    optional
                  </span>
                )}
              </div>
            ))}
          </div>
        )}

        {/* AC-6: the declaration above, and what it actually turned into. */}
        <div className="ports-leases">
          <div className="faint small">live leases · {leases.length}</div>
          {leases.length === 0 ? (
            <span className="faint small">none held right now</span>
          ) : (
            leases.map((l) => (
              <div key={`${l.session}:${l.port}`} className="ports-listrow">
                <span className="mono">{l.port}</span>
                <Pill>{l.name}</Pill>
                <span className="mono faint small">{l.env}</span>
                <span className="ports-lease-session">{l.session}</span>
              </div>
            ))
          )}
        </div>
      </div>
    </Panel>
  );
}
