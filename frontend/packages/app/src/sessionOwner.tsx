import React from "react";

/**
 * Who started a session, relative to the caller.
 *
 * The session list is already scoped server-side by role: a member only ever
 * receives their own sessions, so for them this is a quiet, redundant "you". An
 * owner/admin receives the whole tenant's sessions — this is what lets them tell
 * theirs apart from the team's at a glance. A legacy/MCP row has no creator
 * (`created_by === null`, seen only by admins); it is neither, so show a neutral
 * dash rather than pretending it belongs to "the team". Read-only display —
 * there is no ownership to set here.
 */
export function SessionOwner({
  createdBy,
  meId,
}: {
  createdBy?: string | null;
  meId?: string;
}) {
  if (!createdBy) return <span className="faint mono small">—</span>;
  const mine = createdBy === meId;
  return (
    <span className={`mono small ${mine ? "bright" : "muted"}`}>
      {mine ? "you" : "team"}
    </span>
  );
}
