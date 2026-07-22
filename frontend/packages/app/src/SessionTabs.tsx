// The tab strip above the terminal — VS-Code-style: click to switch, × to
// close the tab (the session keeps running), right-click for the rest, + to
// start new work.
import React, { useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { Pin, Plus, SquareTerminal, X } from "lucide-react";
import { api } from "@nookos/api";
import { useWorkspaceContext } from "./context";
import { useLive } from "./live";
import { useNewWork } from "./newwork";
import { useSessionTabs } from "./sessionTabsStore";
import { askText, notify } from "./dialogs";

interface MenuState {
  id: string;
  x: number;
  y: number;
}

export function SessionTabs({ activeId }: { activeId?: string }) {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const allTabs = useSessionTabs((s) => s.tabs);
  const store = useSessionTabs();
  const sessionStatus = useLive((s) => s.sessionStatus);
  const showNewWork = useNewWork((s) => s.show);
  const selectedWorkspaceId = useWorkspaceContext((s) => s.selectedWorkspaceId);
  const [menu, setMenu] = useState<MenuState | null>(null);

  // Tabs are scoped to the workspace context; "all workspaces" shows every
  // tab, labeled with its workspace so cross-workspace tabs stay tellable.
  // Pinned tabs sort first, like an editor.
  const tabs = (
    selectedWorkspaceId
      ? allTabs.filter((t) => !t.workspaceId || t.workspaceId === selectedWorkspaceId)
      : allTabs
  )
    .slice()
    .sort((a, b) => Number(!!b.pinned) - Number(!!a.pinned));

  if (tabs.length === 0) return null;

  const closeTab = (id: string) => {
    const idx = tabs.findIndex((t) => t.id === id);
    store.close(id);
    if (id === activeId) {
      // Next stop comes from the VISIBLE (filtered) strip.
      const next = tabs[idx + 1] ?? tabs[idx - 1];
      navigate(next && next.id !== id ? `/sessions/${next.id}` : "/sessions");
    }
  };

  /** Rename the session itself, so every viewer sees it — not just this tab. */
  const renameSession = async (id: string, current: string) => {
    const name = await askText({
      title: "Rename session",
      label: "Session name",
      value: current,
      confirmLabel: "rename",
    });
    if (!name || name === current) return;
    store.rename(id, name); // optimistic
    const { error } = await api.PATCH("/api/v1/sessions/{id}", {
      params: { path: { id } },
      body: { name },
    });
    if (error) {
      store.rename(id, current);
      await notify("Rename failed", "The control plane rejected the change.");
      return;
    }
    queryClient.invalidateQueries();
  };

  return (
    <>
      <div className="session-tabs">
        {tabs.map((t) => {
          const st = sessionStatus[t.id];
          const dead = st === "exited" || st === "error" || st === "killed";
          return (
            <div
              key={t.id}
              className={`session-tab${t.id === activeId ? " active" : ""}${
                t.pinned ? " pinned" : ""
              }`}
              onClick={() => navigate(`/sessions/${t.id}`)}
              onContextMenu={(e) => {
                e.preventDefault();
                setMenu({ id: t.id, x: e.clientX, y: e.clientY });
              }}
              onDoubleClick={() => renameSession(t.id, t.name)}
              title={`${t.name} · ${t.runtime}${st ? ` · ${st}` : ""}`}
            >
              <SquareTerminal
                size={12}
                className={`session-tab-icon ${dead ? "err" : "ok"}`}
              />
              {!selectedWorkspaceId && t.workspaceName && (
                <span className="session-tab-ws">{t.workspaceName} /</span>
              )}
              <span className="session-tab-name">{t.name}</span>
              {t.pinned && <Pin size={10} className="session-tab-pin" />}
              <button
                className="session-tab-close"
                title="close tab (session keeps running)"
                onClick={(e) => {
                  e.stopPropagation();
                  closeTab(t.id);
                }}
              >
                <X size={11} />
              </button>
            </div>
          );
        })}
        <button
          className="session-tab-new"
          title="new work"
          onClick={() => showNewWork()}
        >
          <Plus size={13} />
        </button>
      </div>

      {menu && (
        <TabMenu
          x={menu.x}
          y={menu.y}
          onClose={() => setMenu(null)}
          items={[
            { label: "Close", onSelect: () => closeTab(menu.id) },
            {
              label: "Close Others",
              disabled: tabs.length < 2,
              onSelect: () => {
                store.closeOthers(menu.id);
                if (activeId !== menu.id) navigate(`/sessions/${menu.id}`);
              },
            },
            {
              label: "Close to the Right",
              disabled: tabs.findIndex((t) => t.id === menu.id) >= tabs.length - 1,
              onSelect: () =>
                store.closeToTheRight(
                  menu.id,
                  tabs.map((t) => t.id),
                ),
            },
            {
              label: "Close All",
              onSelect: () => {
                store.closeAll(tabs.map((t) => t.id));
                navigate("/sessions");
              },
              divider: true,
            },
            {
              label: tabs.find((t) => t.id === menu.id)?.pinned ? "Unpin" : "Pin",
              onSelect: () => store.togglePin(menu.id),
            },
            {
              label: "Rename Session…",
              onSelect: () => {
                const tab = tabs.find((t) => t.id === menu.id);
                if (tab) renameSession(tab.id, tab.name);
              },
            },
            {
              label: "Copy Session ID",
              divider: true,
              onSelect: () => navigator.clipboard?.writeText(menu.id).catch(() => {}),
            },
          ]}
        />
      )}
    </>
  );
}

export interface MenuItem {
  label: string;
  onSelect(): void;
  disabled?: boolean;
  danger?: boolean;
  /** Draw a separator above this item. */
  divider?: boolean;
}

/** A small context menu that closes on select, outside click, or Escape. */
export function TabMenu({
  x,
  y,
  items,
  onClose,
}: {
  x: number;
  y: number;
  items: MenuItem[];
  onClose(): void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState({ x, y });

  useEffect(() => {
    // Keep the menu on screen when opened near an edge.
    const el = ref.current;
    if (el) {
      const r = el.getBoundingClientRect();
      setPos({
        x: Math.min(x, window.innerWidth - r.width - 8),
        y: Math.min(y, window.innerHeight - r.height - 8),
      });
    }
    const away = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) onClose();
    };
    const key = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    document.addEventListener("mousedown", away);
    document.addEventListener("keydown", key);
    return () => {
      document.removeEventListener("mousedown", away);
      document.removeEventListener("keydown", key);
    };
  }, [x, y, onClose]);

  return (
    <div
      ref={ref}
      className="context-menu"
      style={{ left: pos.x, top: pos.y }}
      onContextMenu={(e) => e.preventDefault()}
    >
      {items.map((item, i) => (
        <React.Fragment key={item.label}>
          {item.divider && i > 0 && <div className="context-menu-sep" />}
          <button
            className={`context-menu-item${item.danger ? " danger" : ""}`}
            disabled={item.disabled}
            onClick={() => {
              onClose();
              item.onSelect();
            }}
          >
            {item.label}
          </button>
        </React.Fragment>
      ))}
    </div>
  );
}
