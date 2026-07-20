// Terminals *inside* a session. A session is one tmux session, and tmux holds
// many windows — so this strip is how a session gets more than one terminal.
// Switching, adding, splitting, renaming and closing all go through the node
// and re-render from the list tmux reports back.
import React, { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Columns2, Plus, Rows2 } from "lucide-react";
import { api } from "@nookos/api";
import { TabMenu } from "./SessionTabs";

export function SessionWindows({ sessionId }: { sessionId: string }) {
  const queryClient = useQueryClient();
  const [menu, setMenu] = useState<{ index: number; x: number; y: number } | null>(
    null,
  );
  const key = ["session-windows", sessionId];

  const { data: windows } = useQuery({
    queryKey: key,
    queryFn: async () =>
      (
        await api.POST("/api/v1/sessions/{id}/windows", {
          params: { path: { id: sessionId } },
          body: { action: "list" },
        })
      ).data ?? [],
    // tmux is the source of truth and the user can also change windows from
    // inside the terminal, so poll gently.
    refetchInterval: 5000,
  });

  const act = async (body: Record<string, unknown>) => {
    const { data } = await api.POST("/api/v1/sessions/{id}/windows", {
      params: { path: { id: sessionId } },
      body: body as never,
    });
    if (data) queryClient.setQueryData(key, data);
  };

  const list = windows ?? [];
  // One unsplit terminal is the normal case — don't clutter the UI for it.
  if (list.length <= 1 && (list[0]?.panes ?? 1) <= 1) {
    return (
      <button
        className="term-strip-add lonely"
        title="new terminal in this session"
        onClick={() => act({ action: "new", cwd: null })}
      >
        <Plus size={12} /> terminal
      </button>
    );
  }

  return (
    <>
      <span className="term-strip">
        {list.map((w) => (
          <button
            key={w.index}
            className={`term-chip${w.active ? " active" : ""}`}
            onClick={() => act({ action: "select", index: w.index })}
            onContextMenu={(e) => {
              e.preventDefault();
              setMenu({ index: w.index, x: e.clientX, y: e.clientY });
            }}
            onDoubleClick={() => {
              const name = window.prompt("Rename terminal", w.name);
              if (name?.trim()) act({ action: "rename", index: w.index, name: name.trim() });
            }}
            title={`${w.name}${(w.panes ?? 1) > 1 ? ` · ${w.panes} panes` : ""}`}
          >
            {w.name}
            {(w.panes ?? 1) > 1 && <span className="faint"> ⋮{w.panes}</span>}
          </button>
        ))}
        <button
          className="term-strip-add"
          title="new terminal in this session"
          onClick={() => act({ action: "new", cwd: null })}
        >
          <Plus size={12} />
        </button>
      </span>

      {menu && (
        <TabMenu
          x={menu.x}
          y={menu.y}
          onClose={() => setMenu(null)}
          items={[
            {
              label: "Split Right",
              onSelect: () => act({ action: "split", vertical: false }),
            },
            {
              label: "Split Down",
              onSelect: () => act({ action: "split", vertical: true }),
            },
            {
              label: "Rename Terminal…",
              divider: true,
              onSelect: () => {
                const w = list.find((x) => x.index === menu.index);
                const name = window.prompt("Rename terminal", w?.name ?? "");
                if (name?.trim())
                  act({ action: "rename", index: menu.index, name: name.trim() });
              },
            },
            {
              label: "Close Terminal",
              danger: true,
              disabled: list.length < 2,
              onSelect: () => act({ action: "close", index: menu.index }),
            },
          ]}
        />
      )}
    </>
  );
}

/** Split buttons for the session panel header. */
export function SplitButtons({ sessionId }: { sessionId: string }) {
  const queryClient = useQueryClient();
  const split = async (vertical: boolean) => {
    const { data } = await api.POST("/api/v1/sessions/{id}/windows", {
      params: { path: { id: sessionId } },
      body: { action: "split", vertical } as never,
    });
    if (data) queryClient.setQueryData(["session-windows", sessionId], data);
  };
  return (
    <>
      <button
        className="btn small icon"
        title="split right"
        onClick={() => split(false)}
      >
        <Columns2 size={13} />
      </button>
      <button
        className="btn small icon"
        title="split down"
        onClick={() => split(true)}
      >
        <Rows2 size={13} />
      </button>
    </>
  );
}
