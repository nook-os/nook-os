import React, { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api, type EventItem } from "@nookos/api";
import { Empty, Panel } from "@nookos/ui";
import { useLive } from "../live";
import { useWorkspaceContext } from "../context";
import { ScopeChip } from "../layout";

function describe(e: EventItem): string {
  const p = e.payload as Record<string, unknown>;
  return (
    (p.title as string) ??
    (p.name as string) ??
    (p.email as string) ??
    (p.detail as string) ??
    (p.message as string) ??
    (p.remote as string) ??
    ""
  );
}

export function ActivityFeed({
  limit = 100,
  workspaceId,
  kind,
}: {
  limit?: number;
  workspaceId?: string;
  kind?: string;
}) {
  const seedActivity = useLive((s) => s.seedActivity);
  const activity = useLive((s) => s.activity);

  const { data } = useQuery({
    queryKey: ["events", workspaceId ?? "all", kind ?? "all"],
    queryFn: async () =>
      (
        await api.GET("/api/v1/events", {
          params: {
            query: { limit, workspace_id: workspaceId, kind },
          },
        })
      ).data,
  });

  useEffect(() => {
    if (data?.events && !workspaceId && !kind) seedActivity(data.events);
  }, [data, seedActivity, workspaceId, kind]);

  // Unfiltered feeds render the live buffer; filtered feeds render the fetch.
  const events =
    !workspaceId && !kind
      ? activity
      : (data?.events ?? []).filter(
          (e) =>
            (!workspaceId || e.workspace_id === workspaceId) &&
            (!kind || e.kind.startsWith(kind)),
        );

  if (events.length === 0) return <Empty>No activity yet.</Empty>;
  return (
    <div>
      {events.slice(0, limit).map((e) => (
        <div className="activity-row" key={e.id}>
          <span className="ts">
            {new Date(e.occurred_at).toLocaleTimeString([], { hour12: false })}
          </span>
          <span className="kind">{e.kind}</span>
          <span className="detail">{describe(e)}</span>
        </div>
      ))}
    </div>
  );
}

const KINDS = ["", "node.", "session.", "task.", "workspace.", "user.", "note."];

export function ActivityPage() {
  const [kind, setKind] = useState("");
  const { selectedWorkspaceId } = useWorkspaceContext();
  return (
    <div className="nook-grid" style={{ gridTemplateColumns: "1fr" }}>
      <Panel
        title="Activity timeline"
        actions={
          <>
            <ScopeChip />{" "}
            <select
              className="input small"
              value={kind}
              onChange={(e) => setKind(e.target.value)}
            >
              {KINDS.map((k) => (
                <option key={k} value={k}>
                  {k || "all kinds"}
                </option>
              ))}
            </select>
          </>
        }
      >
        <ActivityFeed
          limit={200}
          kind={kind || undefined}
          workspaceId={selectedWorkspaceId ?? undefined}
        />
      </Panel>
    </div>
  );
}
