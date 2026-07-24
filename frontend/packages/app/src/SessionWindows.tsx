// Terminals *inside* a session. A session is one tmux session, and tmux holds
// many windows — so this strip is how a session gets more than one terminal.
// Switching, adding, splitting, renaming and closing all go through the node
// and re-render from the list tmux reports back.
import React, { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { CircleDot, Columns2, Loader2, Plus, Rows2, X } from "lucide-react";
import { api } from "@nookos/api";
import { TabMenu } from "./SessionTabs";
import { useLive } from "./live";
import { askText } from "./dialogs";

export function SessionWindows({ sessionId }: { sessionId: string }) {
  const queryClient = useQueryClient();
  const agent = useLive((s) => s.agentState[sessionId]);
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

  return (
    <>
      <span className="term-strip">
        {list.map((w) => (
          <div
            key={w.index}
            role="button"
            tabIndex={0}
            className={`term-chip${w.active ? " active" : ""}`}
            onClick={() => act({ action: "select", index: w.index })}
            onContextMenu={(e) => {
              e.preventDefault();
              setMenu({ index: w.index, x: e.clientX, y: e.clientY });
            }}
            onDoubleClick={async () => {
              const name = await askText({
                title: "Rename terminal",
                value: w.name,
                confirmLabel: "rename",
              });
              if (name) act({ action: "rename", index: w.index, name });
            }}
            title={`${w.name}${(w.panes ?? 1) > 1 ? ` · ${w.panes} panes` : ""}${
              agent && agent.window === w.index ? ` · agent ${agent.state}` : ""
            }`}
          >
            {/* The agent runs in exactly one window; light only that chip so the
                plain shells next to it stay plain. */}
            {agent && agent.window === w.index && agent.state === "running" && (
              <Loader2 size={10} className="term-chip-agent spin running" />
            )}
            {agent && agent.window === w.index && agent.state === "waiting" && (
              <CircleDot size={10} className="term-chip-agent waiting" />
            )}
            {w.name}
            {(w.panes ?? 1) > 1 && <span className="faint"> ⋮{w.panes}</span>}
            {/* Closing ONE terminal needs to be visible. It used to live only
                in the right-click menu, which meant the only obvious way to
                get rid of a terminal was `kill` — and that ends the whole
                session, every terminal in it. Never on the last one: a
                session with no terminals is just a dead session. */}
            {list.length > 1 && (
              <button
                className="term-chip-close"
                title="close this terminal"
                onClick={(e) => {
                  e.stopPropagation();
                  act({ action: "close", index: w.index });
                }}
              >
                <X size={10} />
              </button>
            )}
          </div>
        ))}
        <button
          className="term-strip-add"
          title="new terminal in this session"
          onClick={() => act({ action: "new", cwd: null })}
        >
          <Plus size={12} />
          {list.length <= 1 && <span>terminal</span>}
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
              onSelect: async () => {
                const w = list.find((x) => x.index === menu.index);
                const name = await askText({
                  title: "Rename terminal",
                  value: w?.name ?? "",
                  confirmLabel: "rename",
                });
                if (name) act({ action: "rename", index: menu.index, name });
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
