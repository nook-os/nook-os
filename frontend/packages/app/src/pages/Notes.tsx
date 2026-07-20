import React, { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "@nookos/api";
import { Empty } from "@nookos/ui";

export function NotesPanel({ workspaceId }: { workspaceId: string }) {
  const queryClient = useQueryClient();
  const [draft, setDraft] = useState("");
  const { data: notes } = useQuery({
    queryKey: ["notes", workspaceId],
    queryFn: async () =>
      (
        await api.GET("/api/v1/workspaces/{id}/notes", {
          params: { path: { id: workspaceId } },
        })
      ).data ?? [],
  });

  const rolling = (notes ?? []).find((n) => n.kind === "rolling");

  const append = async () => {
    if (!draft.trim()) return;
    const stamp = new Date().toISOString().slice(0, 16).replace("T", " ");
    const line = `\n- **${stamp}** ${draft.trim()}`;
    if (rolling) {
      await api.PATCH("/api/v1/notes/{id}", {
        params: { path: { id: rolling.id } },
        body: { content_md: rolling.content_md + line },
      });
    } else {
      await api.POST("/api/v1/workspaces/{id}/notes", {
        params: { path: { id: workspaceId } },
        body: { content_md: `# Rolling notes${line}`, kind: "rolling" },
      });
    }
    setDraft("");
    queryClient.invalidateQueries({ queryKey: ["notes", workspaceId] });
  };

  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%" }}>
      <div style={{ flex: 1, overflow: "auto", padding: 10 }}>
        {rolling ? (
          <pre
            className="mono small"
            style={{ whiteSpace: "pre-wrap", color: "var(--nook-fg)" }}
          >
            {rolling.content_md}
          </pre>
        ) : (
          <Empty>No notes yet — knowledge accumulates here.</Empty>
        )}
      </div>
      <div
        style={{
          display: "flex",
          gap: 6,
          padding: 8,
          borderTop: "1px solid var(--nook-border)",
        }}
      >
        <input
          className="input"
          style={{ flex: 1 }}
          placeholder="append a note…"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && append()}
        />
        <button className="btn" onClick={append}>
          add
        </button>
      </div>
    </div>
  );
}
