// The active-tenant control in the status strip. A person in one tenant sees a
// plain label (exactly as before); a person in several gets a menu to switch
// between them. Switching re-scopes the whole app — boards, workspaces,
// sessions, secrets all belong to the active tenant — so it invalidates every
// query rather than reloading the page.
import React, { useEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "react-router-dom";
import { Check, ChevronDown } from "lucide-react";
import { api, type MeResponse } from "@nookos/api";
import { useWorkspaceContext } from "./context";
import { tenantSwitcherModel } from "./tenants";
import { notify } from "./dialogs";

export function TenantSwitcher({ me }: { me: MeResponse }) {
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const ref = useRef<HTMLSpanElement>(null);
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

  // Single-tenant: the plain label the strip has always shown, no affordance.
  if (!model.isMenu) {
    return <span className="faint">tenant: {model.currentName}</span>;
  }

  const switchTo = async (tenantId: string) => {
    setOpen(false);
    if (tenantId === model.currentId || busy) return;
    setBusy(true);
    const { error, response } = await api.POST("/api/v1/me/tenant", {
      body: { tenant_id: tenantId },
    });
    setBusy(false);
    if (error || !response.ok) {
      await notify("Could not switch tenant", JSON.stringify(error ?? {}));
      return;
    }
    // The selected workspace belonged to the old tenant and no longer exists;
    // drop the scope, leave detail routes, then refetch everything so every
    // tenant-scoped surface re-loads against the new active tenant.
    select(null);
    navigate("/");
    queryClient.invalidateQueries();
  };

  return (
    <span className="tenant-switcher" ref={ref}>
      <button
        className="tenant-switcher-btn"
        title="switch tenant"
        disabled={busy}
        onClick={() => setOpen((o) => !o)}
      >
        <span className="faint">tenant:</span> {model.currentName}
        <ChevronDown size={12} />
      </button>
      {open && (
        <div className="tenant-switcher-menu">
          {model.options.map((t) => (
            <button
              key={t.id}
              className={`tenant-switcher-item${t.current ? " current" : ""}`}
              onClick={() => switchTo(t.id)}
            >
              <span className="check">{t.current ? <Check size={12} /> : null}</span>
              <span className="name">{t.name}</span>
              <span className="faint small role">{t.role}</span>
            </button>
          ))}
        </div>
      )}
    </span>
  );
}
