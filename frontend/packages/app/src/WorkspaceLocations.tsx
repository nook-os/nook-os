// Compact one-line summary of where a workspace lives: distinct nodes (not
// one row per checkout), its primary branch, and a "+N worktrees" count —
// instead of a messy list of every node×worktree location.
import React from "react";
import type { WorkspaceLocation } from "@nookos/api";
import { Pill, StatusDot } from "@nookos/ui";

export function WorkspaceLocations({
  locations,
}: {
  locations: WorkspaceLocation[];
}) {
  if (locations.length === 0)
    return <span className="faint small">no checkouts</span>;

  // One entry per node, not per checkout.
  const nodes = [...new Map(locations.map((l) => [l.node_id, l])).values()];
  const primary = locations.find((l) => !l.worktree) ?? locations[0];
  const worktrees = locations.filter((l) => l.worktree).length;

  return (
    <span
      style={{ display: "inline-flex", gap: 10, alignItems: "center", flexWrap: "wrap" }}
    >
      {nodes.map((l) => (
        <span key={l.node_id} className="muted">
          <StatusDot status={l.node_status} /> {l.node_name}
        </span>
      ))}
      {primary?.git_branch && <Pill tone="info">{primary.git_branch}</Pill>}
      {worktrees > 0 && (
        <Pill tone="info">
          +{worktrees} worktree{worktrees === 1 ? "" : "s"}
        </Pill>
      )}
    </span>
  );
}
