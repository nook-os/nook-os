// A tenant's automation switches, thrown from OUTSIDE that tenant.
//
// `loops.enabled` and `sessions.reconcile.enabled` are per-tenant and default
// OFF (MAIN-239). `PUT /settings/{key}` writes to whichever tenant you are
// standing in, so turning loops on for somebody else's team meant switching
// into it first — and until you did, their promoted tickets sat queued forever
// with nothing anywhere saying which switch was off. That is what "the loops
// didn't fire for my PM" looks like from the operator's side: not a broken
// loop, an un-thrown switch in a team you were not looking at.
//
// Two toggles on the tenant row, so the state is visible for every team at once
// rather than being a thing you go and check one team at a time.
import React from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type Schemas } from "@nookos/api";

type Switches = Schemas["TenantSwitches"];

export function TenantSwitches({ tenantId, slug }: { tenantId: string; slug: string }) {
  const queryClient = useQueryClient();

  const { data, isLoading } = useQuery<Switches | null>({
    queryKey: ["operator", "switches", tenantId],
    queryFn: async () =>
      ((
        await api.GET("/api/v1/operator/tenants/{id}/switches", {
          params: { path: { id: tenantId } },
        })
      ).data as Switches | undefined) ?? null,
  });

  const flip = useMutation({
    mutationFn: async (v: { switch: string; enabled: boolean }) => {
      const { error } = await api.POST("/api/v1/operator/tenants/{id}/switches", {
        params: { path: { id: tenantId } },
        body: v,
      });
      if (error) throw new Error("refused");
    },
    onSuccess: () =>
      queryClient.invalidateQueries({ queryKey: ["operator", "switches", tenantId] }),
  });

  if (isLoading || !data) return <span className="faint small">—</span>;

  const toggle = (key: "loops" | "reconcile", on: boolean, label: string, why: string) => (
    <button
      className={`task-chip ${on ? "on" : ""}`}
      disabled={flip.isPending}
      title={`${why} — ${on ? "on" : "off"} for ${slug}. Click to turn ${on ? "off" : "on"}.`}
      aria-label={`${label} for ${slug}: ${on ? "on" : "off"}`}
      onClick={() => flip.mutate({ switch: key, enabled: !on })}
    >
      {label}
    </button>
  );

  return (
    <span style={{ display: "inline-flex", gap: 4 }}>
      {toggle(
        "loops",
        data.loops_enabled,
        "loops",
        "whether this team's promoted tickets are dispatched to a node",
      )}
      {toggle(
        "reconcile",
        data.reconcile_enabled,
        "reconcile",
        "whether this team's workspaces are converged onto its nodes",
      )}
    </span>
  );
}
