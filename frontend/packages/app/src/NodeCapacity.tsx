// How many loop jobs a machine runs at once (MAIN-508) — the port range's twin,
// and here for the same reason: the number lived only in the node's process
// environment, so changing it meant a shell on the box and `systemctl restart
// nook-node`, which strands every in-flight streaming build. Setting it here
// costs nothing that is running.
import React, { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type Schemas } from "@nookos/api";
import { Panel, Pill } from "@nookos/ui";

type Capacity = Schemas["NodeCapacity"];

const SOURCE_LABEL: Record<string, string> = {
  host: "pinned on the machine",
  operator: "set here",
  node: "reported by the node",
  default: "no value reported",
};

export function NodeCapacity({
  nodeId,
  canEdit,
}: {
  nodeId: string;
  /** Only the machine's OWNER may set it — the server enforces it and 403s
   *  otherwise; this just avoids offering a control that cannot work. */
  canEdit: boolean;
}) {
  const queryClient = useQueryClient();
  const [editing, setEditing] = useState(false);
  const [jobs, setJobs] = useState("");
  const [busy, setBusy] = useState(false);

  const { data } = useQuery<Capacity | null>({
    queryKey: ["capacity", nodeId],
    queryFn: async () =>
      ((await api.GET("/api/v1/nodes/{id}/capacity", { params: { path: { id: nodeId } } }))
        .data as Capacity | undefined) ?? null,
  });

  useEffect(() => {
    if (!editing || !data) return;
    setJobs(data.operator === null || data.operator === undefined ? "" : String(data.operator));
  }, [editing, data]);

  const save = async () => {
    setBusy(true);
    // Blank clears the override. `0` must survive that test — it is the cordon,
    // and reading it as "empty" would turn the one deliberate stop into a
    // fallback to whatever the machine happens to say.
    const text = jobs.trim();
    const { error } = await api.PUT("/api/v1/nodes/{id}/capacity", {
      params: { path: { id: nodeId } },
      body: { max_loop_jobs: text === "" ? null : Number(text) },
    });
    setBusy(false);
    if (error) return; // the global write-failure toast carries the server's message
    setEditing(false);
    queryClient.invalidateQueries({ queryKey: ["capacity", nodeId] });
    // The nodes table prints the effective number too, so it must not keep
    // showing the old one after a change made on this page.
    queryClient.invalidateQueries({ queryKey: ["nodes"] });
  };

  const source = data?.source ?? "default";
  const pinned = data?.pinned ?? false;

  return (
    <Panel
      title="Loop capacity"
      actions={
        canEdit &&
        !pinned &&
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
            edit
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
              placeholder="jobs"
              aria-label="concurrent loop jobs"
              value={jobs}
              onChange={(e) => setJobs(e.target.value)}
            />
            <span className="faint small">
              {data?.advertised === null || data?.advertised === undefined
                ? "blank to use the node's own"
                : `blank to use the node's own ${data.advertised}`}
              {" · 0 cordons this machine"}
            </span>
          </div>
        ) : (
          <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
            <span className="mono">{data ? data.effective : "—"}</span>
            <span className="faint small">
              {data?.effective === 0 ? "jobs — cordoned" : "concurrent loop jobs"}
            </span>
            {/* Where the number came from. A capacity with no provenance cannot
                answer the question this panel exists for — "why is only one
                thing building" — because "small machine" and "somebody
                cordoned it" print identically. */}
            <Pill>{SOURCE_LABEL[source] ?? source}</Pill>
          </div>
        )}

        {data?.effective === 0 && !editing && (
          <span className="faint small">
            Work already running here finishes; nothing new is placed. This is the
            per-node stop, not a busy machine.
          </span>
        )}

        {/* The precedence, said where somebody is about to rely on it. */}
        {pinned ? (
          <span className="faint small">
            This host pins its own capacity (NOOK_MAX_LOOP_JOBS_PINNED on the
            machine), so it cannot be set from here — unset that there first.
            {data?.operator !== null && data?.operator !== undefined && (
              <> A value of {data.operator} is stored here and currently overruled.</>
            )}
          </span>
        ) : (
          <span className="faint small">
            A number set here wins over the machine's NOOK_MAX_LOOP_JOBS, and takes
            effect at the next dispatch poll — nothing restarts, so builds running
            on this node are undisturbed.
          </span>
        )}
      </div>
    </Panel>
  );
}
