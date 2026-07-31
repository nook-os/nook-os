// Set a machine's labels and taints (MAIN-319 AC-2).
//
// These are the inputs placement reads (MAIN-314): a LABEL says what a node is,
// a TAINT says what it refuses unless the work tolerates it. Keeping them
// visibly apart in the UI is the same reason they are separate columns — "has
// X" and "refuses X" must never become the same question.
import React, { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus, X } from "lucide-react";
import { api } from "@nookos/api";
import { Panel, Pill } from "@nookos/ui";
import { notify } from "./dialogs";

type Pair = { k: string; v: string };

export function NodePlacement({
  nodeId,
  canEdit,
}: {
  nodeId: string;
  /** Only the machine's OWNER may set these — the server enforces it and 403s
   *  otherwise; this just avoids offering a button that cannot work. */
  canEdit: boolean;
}) {
  const queryClient = useQueryClient();
  const [editing, setEditing] = useState(false);
  const [labels, setLabels] = useState<Pair[]>([]);
  const [taints, setTaints] = useState<Pair[]>([]);
  const [busy, setBusy] = useState(false);

  const { data } = useQuery({
    queryKey: ["placement", nodeId],
    queryFn: async () =>
      (
        await api.GET("/api/v1/nodes/{id}/placement", {
          params: { path: { id: nodeId } },
        })
      ).data ?? null,
  });

  useEffect(() => {
    if (!editing || !data) return;
    // The editor holds the CUSTOM labels only. `labels` is the merged view
    // including the derived `os`/`arch`, and round-tripping those back would be
    // asking the server to store a value it refuses — it derives them from what
    // the machine reports, precisely so they cannot drift.
    setLabels(Object.entries(data.custom_labels ?? {}).map(([k, v]) => ({ k, v })));
    setTaints((data.taints ?? []).map((t) => ({ k: t.key, v: t.effect })));
  }, [editing, data]);

  const save = async () => {
    setBusy(true);
    const { error } = await api.PUT("/api/v1/nodes/{id}/placement", {
      params: { path: { id: nodeId } },
      body: {
        labels: Object.fromEntries(
          labels.filter((p) => p.k.trim() && p.v.trim()).map((p) => [p.k.trim(), p.v.trim()]),
        ),
        taints: taints
          .filter((p) => p.k.trim())
          .map((p) => ({ key: p.k.trim(), effect: p.v.trim() || "NoSchedule" })),
      },
    });
    setBusy(false);
    if (error) {
      await notify("The control plane refused that", JSON.stringify(error));
      return;
    }
    setEditing(false);
    queryClient.invalidateQueries({ queryKey: ["placement", nodeId] });
  };

  const rows = (list: Pair[], set: (r: Pair[]) => void, kp: string, vp: string) => (
    <>
      {list.map((p, i) => (
        <div key={i} className="policy-pair">
          <input
            className="input small"
            placeholder={kp}
            value={p.k}
            onChange={(e) => set(list.map((r, j) => (i === j ? { ...r, k: e.target.value } : r)))}
          />
          <input
            className="input small"
            placeholder={vp}
            value={p.v}
            onChange={(e) => set(list.map((r, j) => (i === j ? { ...r, v: e.target.value } : r)))}
          />
          <button className="btn small icon" title="remove" onClick={() => set(list.filter((_, j) => j !== i))}>
            <X size={11} />
          </button>
        </div>
      ))}
      <button className="btn small" onClick={() => set([...list, { k: "", v: "" }])}>
        <Plus size={11} /> add
      </button>
    </>
  );

  const derived = Object.entries(data?.labels ?? {}).filter(
    ([k]) => !(k in (data?.custom_labels ?? {})),
  );

  return (
    <Panel
      title="Placement"
      actions={
        canEdit && !editing ? (
          <button className="btn small" onClick={() => setEditing(true)}>
            edit
          </button>
        ) : null
      }
      style={{ gridColumn: "1 / span 2" }}
    >
      {!editing ? (
        <div className="policy-summary small" style={{ padding: 10 }}>
          <div style={{ display: "flex", flexWrap: "wrap", gap: 6, alignItems: "center" }}>
            <span className="faint">labels</span>
            {/* Derived first and marked as such: they come from what the machine
                reports and cannot be set, so an operator hunting for why they
                cannot edit `os` has the answer on screen. */}
            {derived.map(([k, v]) => (
              <Pill key={k} tone="dim" title="derived from what the machine reports — not editable">
                {k}={v}
              </Pill>
            ))}
            {Object.entries(data?.custom_labels ?? {}).map(([k, v]) => (
              <Pill key={k} tone="accent">
                {k}={v}
              </Pill>
            ))}
            {derived.length === 0 && Object.keys(data?.custom_labels ?? {}).length === 0 && (
              <span className="faint">none</span>
            )}
          </div>
          <div style={{ display: "flex", flexWrap: "wrap", gap: 6, alignItems: "center", marginTop: 8 }}>
            <span className="faint">taints</span>
            {(data?.taints ?? []).length === 0 ? (
              <span className="faint">none — this machine accepts any work</span>
            ) : (
              (data?.taints ?? []).map((t) => (
                <Pill key={t.key} tone="warn" title="work must tolerate this to be placed here">
                  {t.key}:{t.effect}
                </Pill>
              ))
            )}
          </div>
        </div>
      ) : (
        <div className="policy-editor">
          <div className="small">
            custom labels <span className="faint">(what this machine IS)</span>
            {rows(labels, setLabels, "gpu", "yes")}
          </div>
          <div className="small">
            taints <span className="faint">(what it REFUSES unless tolerated)</span>
            {rows(taints, setTaints, "key", "NoSchedule")}
          </div>
          <div className="policy-actions">
            <button className="btn primary small" disabled={busy} onClick={save}>
              save
            </button>
            <button className="btn small" disabled={busy} onClick={() => setEditing(false)}>
              cancel
            </button>
          </div>
        </div>
      )}
    </Panel>
  );
}
