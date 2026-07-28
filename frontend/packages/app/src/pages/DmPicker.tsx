// The new-DM people picker (MAIN-113 AC-5). Lists everyone in the caller's
// org(s) — the only people a DM may reach — and opens (or reuses) a DM with the
// chosen set. Reuses the shared modal chrome, like ChannelManager.
import React, { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Check, X } from "lucide-react";
import { listPeople, openDm, type DmSummary, type PersonRef } from "@nookos/api";
import { notify } from "../dialogs";

export function DmPicker({
  onClose,
  onOpened,
}: {
  onClose: () => void;
  onOpened: (dm: DmSummary) => void;
}) {
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [filter, setFilter] = useState("");
  const [busy, setBusy] = useState(false);

  const q = useQuery({ queryKey: ["chat", "people"], queryFn: listPeople });
  const people = q.data ?? [];

  const shown = useMemo(() => {
    const f = filter.trim().toLowerCase();
    return f
      ? people.filter((p) => p.display_name.toLowerCase().includes(f))
      : people;
  }, [people, filter]);

  const toggle = (id: string) =>
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const start = async () => {
    if (selected.size === 0) return;
    setBusy(true);
    try {
      const dm = await openDm([...selected]);
      onOpened(dm);
    } catch (e) {
      await notify("Could not open DM", e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="modal-backdrop" onMouseDown={onClose}>
      <div
        className="modal chan-manage"
        role="dialog"
        aria-label="New direct message"
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="modal-header">
          New direct message
          <button className="btn small icon chan-manage-close" onClick={onClose} aria-label="close">
            <X size={13} />
          </button>
        </div>
        <div className="modal-body">
          <input
            className="chan-input"
            value={filter}
            placeholder="Filter people…"
            aria-label="filter people"
            onChange={(e) => setFilter(e.target.value)}
          />
          <div className="chan-manage-list">
            {q.isLoading ? (
              <div className="faint small">Loading…</div>
            ) : shown.length === 0 ? (
              <div className="faint small">No one to message.</div>
            ) : (
              shown.map((p: PersonRef) => (
                <button
                  key={p.person_id}
                  type="button"
                  className={`chan-manage-row${selected.has(p.person_id) ? " active" : ""}`}
                  onClick={() => toggle(p.person_id)}
                >
                  <span className="chan-manage-name">{p.display_name}</span>
                  {selected.has(p.person_id) && <Check size={12} />}
                </button>
              ))
            )}
          </div>
          <div className="chan-manage-create dm-create">
            <button
              className="btn small primary"
              onClick={start}
              disabled={busy || selected.size === 0}
            >
              Start chat{selected.size > 1 ? ` (${selected.size})` : ""}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
