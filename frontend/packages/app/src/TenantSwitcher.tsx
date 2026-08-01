// Switching teams, from the top bar, in one click or none.
//
// This control used to live in the 24px status strip at the bottom edge of the
// window, and it only appeared at all for somebody already in several tenants.
// For a person who switches teams constantly that is the worst placement in the
// app — far from the eye, far from the pointer, and easy enough to miss that the
// Team page became the way people did it. Switching teams is not an occasional
// settings change; for anyone working across orgs it is the most frequent
// navigation there is, so it belongs beside the workspace switcher in the top
// bar.
//
// Switching re-scopes the whole app — boards, workspaces, sessions, secrets all
// belong to the active tenant — so it invalidates every query rather than
// reloading the page.
import React, { useEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "react-router-dom";
import { Building2, Check, ChevronDown } from "lucide-react";
import { api, type MeResponse } from "@nookos/api";
import { useWorkspaceContext } from "./context";
import { tenantSwitcherModel } from "./tenants";
import { notify } from "./dialogs";

export function TenantSwitcher({ me }: { me: MeResponse }) {
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const { select } = useWorkspaceContext();
  const model = tenantSwitcherModel(me);

  useEffect(() => {
    const close = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", close);
    return () => document.removeEventListener("mousedown", close);
  }, []);

  const switchTo = async (tenantId: string) => {
    setOpen(false);
    if (tenantId === model.currentId || busy) return;
    setBusy(true);
    const { error, response } = await api.POST("/api/v1/me/tenant", {
      body: { tenant_id: tenantId },
    });
    setBusy(false);
    if (error || !response.ok) {
      await notify("Could not switch team", JSON.stringify(error ?? {}));
      return;
    }
    // The selected workspace belonged to the old tenant and no longer exists;
    // drop the scope, leave detail routes, then refetch everything so every
    // tenant-scoped surface re-loads against the new active tenant.
    select(null);
    navigate("/");
    queryClient.invalidateQueries();
  };

  // Ctrl+Shift+T cycles to the next team — the zero-click path, which is the
  // case this control exists to serve. Two teams is the common shape and
  // cycling lands straight on the other one; with more it walks the list in
  // order, so repeating the chord stays predictable.
  //
  // Bound only when there IS somewhere to go, so a single-team deployment never
  // swallows the chord.
  useEffect(() => {
    if (!model.isMenu) return;
    const onKey = (e: KeyboardEvent) => {
      if (!e.ctrlKey || !e.shiftKey || e.key.toLowerCase() !== "t") return;
      // Never steal the chord from a field somebody is typing in.
      const t = e.target as HTMLElement | null;
      if (t && /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName)) return;
      if (t?.isContentEditable) return;
      e.preventDefault();
      const i = model.options.findIndex((o) => o.current);
      const next = model.options[(i + 1) % model.options.length];
      if (next) void switchTo(next.id);
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [model.isMenu, model.currentId, busy]);

  // One team: a label, not a menu. An affordance leading to a list of one is
  // worse than none — but it stays in the top bar rather than vanishing, so
  // "which team am I in" has the same answer in the same place for everybody.
  if (!model.isMenu) {
    return (
      <div className="team-switcher">
        <span className="team-switcher-btn static" title="your team">
          <Building2 size={14} />
          <span className="name">{model.currentName}</span>
        </span>
      </div>
    );
  }

  return (
    <div className="team-switcher" ref={ref}>
      <button
        className="team-switcher-btn"
        title={`switch team — ${model.options.length} to choose from (Ctrl+Shift+T)`}
        aria-label="switch team"
        disabled={busy}
        onClick={() => setOpen((o) => !o)}
      >
        <Building2 size={14} />
        <span className="name">{model.currentName}</span>
        <ChevronDown size={13} />
      </button>
      {open && (
        <div className="team-switcher-menu">
          {model.options.map((t) => (
            <button
              key={t.id}
              className={`team-switcher-item${t.current ? " current" : ""}`}
              onClick={() => switchTo(t.id)}
            >
              <span className="check">{t.current ? <Check size={12} /> : null}</span>
              <span className="name">{t.name}</span>
              <span className="faint small role">{t.role}</span>
            </button>
          ))}
          <div className="team-switcher-hint faint small">Ctrl+Shift+T to cycle</div>
        </div>
      )}
    </div>
  );
}
