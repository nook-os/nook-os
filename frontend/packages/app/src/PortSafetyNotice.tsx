// "This workspace has not said what it binds" (MAIN-361 AC-5/AC-6/AC-7).
//
// The same sentence in two places: on the workspace, where it explains a cap
// somebody is living with, and in the add-workspace flow, where it warns before
// they finish. One component so the two cannot drift into saying different
// things about the same rule.
import React, { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "@nookos/api";
import { fileOrFindPortsTicket, isPortCapped } from "./portSafety";
import { notify } from "./dialogs";

/** The rule, in the one sentence somebody actually needs. Exported because the
 *  add-workspace flow states it BEFORE a workspace exists to ask about. */
export const PORT_CAP_SENTENCE =
  "A workspace that has not declared its ports is limited to one session per " +
  "node — a second one would bind whatever the app hardcodes, and so would the " +
  "first.";

/** The banner on a workspace that is actually capped right now.
 *
 *  Renders nothing when it is not, rather than a reassuring green box: this is
 *  a condition to fix, and a permanent widget saying "fine" would be noise on
 *  every other workspace. */
export function PortSafetyNotice({
  workspaceId,
  workspaceName,
}: {
  workspaceId: string;
  workspaceName: string;
}) {
  const [busy, setBusy] = useState(false);

  const { data: status } = useQuery({
    queryKey: ["reconcile-status", workspaceId],
    queryFn: async () =>
      (
        await api.GET("/api/v1/workspaces/{id}/reconcile-status", {
          params: { path: { id: workspaceId } },
        })
      ).data ?? null,
  });

  if (!isPortCapped(status)) return null;

  const file = async () => {
    setBusy(true);
    const r = await fileOrFindPortsTicket(workspaceId, workspaceName);
    setBusy(false);
    if (!r) return;
    // Filing twice surfaces the first rather than making a second (AC-7), and
    // says which happened — otherwise a second click looks like it did nothing.
    await notify(
      r.existed ? `Already filed — ${r.key}` : `Filed ${r.key}`,
      r.existed
        ? "A ticket for this repo's ports is already in Triage."
        : "It is in Triage without agent-ready — promote it when you want it built.",
    );
  };

  return (
    <div
      className="small"
      style={{
        color: "var(--nook-warn)",
        border: "1px solid color-mix(in srgb, var(--nook-warn) 40%, transparent)",
        borderRadius: "var(--nook-radius)",
        padding: "6px 8px",
        display: "flex",
        gap: 8,
        alignItems: "center",
        flexWrap: "wrap",
      }}
    >
      <span>
        <strong>Limited to one session per node.</strong> {PORT_CAP_SENTENCE} Declare
        this repo's listeners — a committed <span className="mono">.nook.toml</span>{" "}
        or the ports panel — and the limit lifts by itself.
      </span>
      <button className="btn small" disabled={busy} onClick={file}>
        file a ticket
      </button>
    </div>
  );
}
