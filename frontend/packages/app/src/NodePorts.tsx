// A machine's port range and who is holding what (MAIN-301 AC-5/AC-6).
//
// Two worktrees of one app used to fight over a hardcoded port, so only one
// could run. Sessions lease ports from this range instead, one per listener the
// WORKSPACE declared, each delivered as the env var that workspace named. This
// surface exists because the moment a human needs it — a range that is wrong
// for the machine, a lease that looks stuck — they need to see and change it
// without editing a config file on a box they may not have a shell on.
import React, { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type Schemas } from "@nookos/api";
import { Panel, Pill } from "@nookos/ui";

type Ports = Schemas["NodePorts"];

export function NodePorts({
  nodeId,
  canEdit,
}: {
  nodeId: string;
  /** Only the machine's OWNER may set the range or release a lease — the
   *  server enforces it and 403s otherwise; this just avoids offering a
   *  control that cannot work. */
  canEdit: boolean;
}) {
  const queryClient = useQueryClient();
  const [editing, setEditing] = useState(false);
  const [start, setStart] = useState("");
  const [end, setEnd] = useState("");
  const [busy, setBusy] = useState(false);
  const [editingExcl, setEditingExcl] = useState(false);
  const [excl, setExcl] = useState("");

  // Typed explicitly rather than inferred: the generated client's response
  // union is wide enough that a bare `.data` loses the shape, and a `leases`
  // that silently becomes `any[]` is how a rename ships broken.
  const { data } = useQuery<Ports | null>({
    queryKey: ["ports", nodeId],
    queryFn: async () =>
      ((await api.GET("/api/v1/nodes/{id}/ports", { params: { path: { id: nodeId } } }))
        .data as Ports | undefined) ?? null,
  });

  useEffect(() => {
    if (!editing || !data) return;
    setStart(data.range ? String(data.range.start) : "");
    setEnd(data.range ? String(data.range.end) : "");
  }, [editing, data]);

  useEffect(() => {
    if (!editingExcl || !data) return;
    setExcl((data.excluded ?? []).join(", "));
  }, [editingExcl, data]);

  const refresh = () => queryClient.invalidateQueries({ queryKey: ["ports", nodeId] });

  // `4510, 4700-4705` → the ports it names. Ranges are accepted because a
  // machine's foreign occupants come in blocks as often as singly.
  const parsePorts = (text: string): number[] => {
    const out: number[] = [];
    for (const part of text.split(",").map((p) => p.trim()).filter(Boolean)) {
      const dash = part.indexOf("-");
      if (dash > 0) {
        const a = Number(part.slice(0, dash).trim());
        const b = Number(part.slice(dash + 1).trim());
        if (Number.isFinite(a) && Number.isFinite(b) && b >= a) {
          for (let p = a; p <= b; p++) out.push(p);
        }
      } else {
        const n = Number(part);
        if (Number.isFinite(n)) out.push(n);
      }
    }
    return [...new Set(out)].sort((a, b) => a - b);
  };

  const saveExclusions = async () => {
    setBusy(true);
    // Its OWN endpoint. The range body reads "neither start nor end" as CLEAR
    // THE RANGE, so posting exclusions there would silently unset it.
    const { error } = await api.PUT("/api/v1/nodes/{id}/ports/exclusions", {
      params: { path: { id: nodeId } },
      body: { ports: parsePorts(excl) },
    });
    setBusy(false);
    if (error) return; // the server refuses a list that empties the range
    setEditingExcl(false);
    refresh();
  };

  const save = async () => {
    setBusy(true);
    // Both blank clears the override; the server refuses a half-set range, so
    // the two fields travel together rather than one being "kept".
    const blank = !start.trim() && !end.trim();
    const { error } = await api.PUT("/api/v1/nodes/{id}/ports", {
      params: { path: { id: nodeId } },
      body: blank
        ? { start: null, end: null }
        : { start: Number(start.trim()), end: Number(end.trim()) },
    });
    setBusy(false);
    if (error) return; // the global write-failure toast carries the server's message
    setEditing(false);
    refresh();
  };

  // No confirmation: releasing is not destructive — the session keeps running
  // and the port is immediately re-leasable. A dialog here would be ceremony
  // in front of the one action a stuck lease needs.
  const release = async (sessionId: string) => {
    setBusy(true);
    await api.DELETE("/api/v1/nodes/{id}/leases/{session}", {
      params: { path: { id: nodeId, session: sessionId } },
    });
    setBusy(false);
    refresh();
  };

  const range = data?.range ?? null;
  const leases = data?.leases ?? [];
  const excluded = data?.excluded ?? [];
  // Only the ones INSIDE the range cost capacity; 443 and 5432 are the ports
  // people think of first and the allocator was never going to hand them out.
  const bitingCount = range
    ? excluded.filter((p) => p >= range.start && p <= range.end).length
    : 0;
  const usable = range ? range.end - range.start + 1 - bitingCount : 0;

  return (
    <Panel
      title="Ports"
      actions={
        canEdit &&
        (editing ? (
          <>
            <button className="btn small" disabled={busy} onClick={save}>
              save
            </button>
            <button className="btn small" disabled={busy} onClick={() => setEditing(false)}>
              cancel
            </button>
          </>
        ) : (
          <button className="btn small" onClick={() => setEditing(true)}>
            edit range
          </button>
        ))
      }
    >
      <div style={{ padding: 10, display: "grid", gap: 8 }}>
        {editing ? (
          <div style={{ display: "flex", gap: 6, alignItems: "center" }}>
            <input
              className="input"
              style={{ width: 90 }}
              placeholder="start"
              aria-label="port range start"
              value={start}
              onChange={(e) => setStart(e.target.value)}
            />
            <span className="faint">–</span>
            <input
              className="input"
              style={{ width: 90 }}
              placeholder="end"
              aria-label="port range end"
              value={end}
              onChange={(e) => setEnd(e.target.value)}
            />
            <span className="faint small">
              {data?.advertised
                ? `blank to use the node's own ${data.advertised.start}–${data.advertised.end}`
                : "blank to lease no ports"}
            </span>
          </div>
        ) : (
          <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
            {range ? (
              <>
                <span className="mono">
                  {range.start}–{range.end}
                </span>
                {/* Where the range came from. Two numbers with no provenance
                    leave you unable to tell "the machine says so" from "someone
                    typed this", which is the whole question when it is wrong. */}
                <Pill>{data?.source === "operator" ? "set here" : "reported by the node"}</Pill>
              </>
            ) : (
              <span className="faint small">
                no range — this machine leases no ports, so sessions here start
                with none of the variables their workspace asked for
              </span>
            )}
          </div>
        )}

        {/* Deliberately its own block, above leases and styled apart. The two
            are unavailable for opposite reasons and on opposite timescales: a
            LEASE is a port working correctly, held by us, back on its own when
            the session dies. An EXCLUSION is a standing decision that nothing
            reclaims. Showing them as one list of "unavailable ports" is what
            would let somebody treat a busy port as a broken one. */}
        <div>
          <div
            className="faint small"
            style={{ marginBottom: 4, display: "flex", gap: 8, alignItems: "center" }}
          >
            <span>excluded · {excluded.length}</span>
            {canEdit &&
              (editingExcl ? (
                <>
                  <button className="btn small" disabled={busy} onClick={saveExclusions}>
                    save
                  </button>
                  <button
                    className="btn small"
                    disabled={busy}
                    onClick={() => setEditingExcl(false)}
                  >
                    cancel
                  </button>
                </>
              ) : (
                <button className="btn small" onClick={() => setEditingExcl(true)}>
                  edit
                </button>
              ))}
          </div>
          {editingExcl ? (
            <div style={{ display: "grid", gap: 4 }}>
              <input
                className="input"
                placeholder="4510, 4700-4705"
                aria-label="excluded ports"
                value={excl}
                onChange={(e) => setExcl(e.target.value)}
              />
              <span className="faint small">
                Ports something else owns on this machine — including one that is
                not listening right now but will be after a reboot, which is the
                case nothing else catches. Blank to clear.
              </span>
            </div>
          ) : excluded.length === 0 ? (
            <span className="faint small">none — the whole range is available</span>
          ) : (
            <div style={{ display: "flex", gap: 6, flexWrap: "wrap", alignItems: "center" }}>
              <span className="mono">{excluded.join(", ")}</span>
              {/* Capacity is what an operator is really choosing: a workspace
                  leases one port per declared listener, all at once. */}
              {range && (
                <span className="faint small">
                  {usable} of {range.end - range.start + 1} usable
                </span>
              )}
            </div>
          )}
        </div>

        <div>
          <div className="faint small" style={{ marginBottom: 4 }}>
            leases · {leases.length}
          </div>
          {leases.length === 0 && <span className="faint small">none held</span>}
          {/* Keyed on session AND port: one session holds one lease per declared
              listener now, so the session id alone is no longer unique here. */}
          {leases.map((l) => (
            <div
              key={`${l.session_id}:${l.port}`}
              style={{ display: "flex", gap: 8, alignItems: "center", padding: "2px 0" }}
            >
              <span className="mono">{l.port}</span>
              {/* WHICH listener, not just that something holds it — with three
                  ports on one session the number alone says nothing. */}
              <Pill>{l.name}</Pill>
              <span className="mono faint small">{l.env}</span>
              <span style={{ flex: 1, overflow: "hidden", textOverflow: "ellipsis" }}>
                {l.session_name}
              </span>
              <span className="faint small">{l.status}</span>
              {canEdit && (
                <button
                  className="btn small"
                  disabled={busy}
                  title="hand this session's ports back without ending it"
                  aria-label={`release ${l.port}`}
                  onClick={() => release(l.session_id)}
                >
                  release
                </button>
              )}
            </div>
          ))}
        </div>
      </div>
    </Panel>
  );
}
