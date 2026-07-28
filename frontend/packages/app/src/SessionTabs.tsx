// The tab strip above the terminal — VS-Code-style: click to switch, × to
// close the tab (the session keeps running), right-click for the rest, + to
// start new work.
import React, { useState } from "react";
import { useNavigate } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { CircleDot, Loader2, Pin, Plus, SquareTerminal, X } from "lucide-react";
import { api } from "@nookos/api";
import { useWorkspaceContext } from "./context";
import { useLive } from "./live";
import { useNewWork } from "./newwork";
import { useSessionTabs } from "./sessionTabsStore";
import { useTabHotkeys } from "./tabHotkeys";
import { askText, notify } from "./dialogs";
import { ContextMenuRegion, type ContextMenuItem } from "./contextMenu";

export function SessionTabs({ activeId }: { activeId?: string }) {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const allTabs = useSessionTabs((s) => s.tabs);
  const store = useSessionTabs();
  const sessionStatus = useLive((s) => s.sessionStatus);
  const agentState = useLive((s) => s.agentState);
  const showNewWork = useNewWork((s) => s.show);
  const selectedWorkspaceId = useWorkspaceContext((s) => s.selectedWorkspaceId);
  // Drag-to-reorder state: which tab is being dragged, and where the insertion
  // line currently sits (a target tab and whether it drops after it). Both null
  // when nothing is dragging.
  const [dragId, setDragId] = useState<string | null>(null);
  const [dropAt, setDropAt] = useState<{ id: string; after: boolean } | null>(null);

  const endDrag = () => {
    setDragId(null);
    setDropAt(null);
  };

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

  // Chrome-style Ctrl+Tab / Ctrl+Cmd-number switching over exactly this visible
  // list (desktop only). Called before the early return so the hook order is
  // stable across renders; with an empty list it simply has nothing to switch.
  useTabHotkeys(
    tabs.map((t) => t.id),
    activeId,
    navigate,
  );

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

  // The tab's right-click menu, as items for the shared primitive (MAIN-168).
  const tabMenu = (t: (typeof tabs)[number]): ContextMenuItem[] => {
    const idx = tabs.findIndex((x) => x.id === t.id);
    return [
      { label: "Close", onSelect: () => closeTab(t.id) },
      {
        label: "Close Others",
        disabled: tabs.length < 2,
        onSelect: () => {
          store.closeOthers(t.id);
          if (activeId !== t.id) navigate(`/sessions/${t.id}`);
        },
      },
      {
        label: "Close to the Right",
        disabled: idx >= tabs.length - 1,
        onSelect: () =>
          store.closeToTheRight(
            t.id,
            tabs.map((x) => x.id),
          ),
      },
      { separator: true },
      {
        label: "Close All",
        onSelect: () => {
          store.closeAll(tabs.map((x) => x.id));
          navigate("/sessions");
        },
      },
      { label: t.pinned ? "Unpin" : "Pin", onSelect: () => store.togglePin(t.id) },
      { label: "Rename Session…", onSelect: () => renameSession(t.id, t.name) },
      { separator: true },
      {
        label: "Copy Session ID",
        onSelect: () => void navigator.clipboard?.writeText(t.id).catch(() => {}),
      },
    ];
  };

  return (
    <>
      <div className="session-tabs">
        {tabs.map((t) => {
          const st = sessionStatus[t.id];
          const dead = st === "exited" || st === "error" || st === "killed";
          const dragged = dragId ? tabs.find((x) => x.id === dragId) : null;
          // A drop is only legal within the same pin group (AC-3), so the
          // insertion line and the drop itself are gated on it.
          const sameGroup = dragged ? !!dragged.pinned === !!t.pinned : false;
          const dropHere = dropAt?.id === t.id ? dropAt : null;
          // A live agent's state trumps the plain terminal glyph: a spinner
          // while it runs, a "needs you" dot when it blocks. A dead session is
          // dead regardless of the last thing its agent said.
          const agent = dead ? undefined : agentState[t.id]?.state;
          return (
            // Right-click → the tab menu, via the shared primitive (MAIN-168).
            <ContextMenuRegion
              key={t.id}
              style={{ display: "contents" }}
              items={() => tabMenu(t)}
            >
            <div
              className={
                `session-tab${t.id === activeId ? " active" : ""}` +
                `${t.pinned ? " pinned" : ""}` +
                `${dragId === t.id ? " dragging" : ""}` +
                `${dropHere && !dropHere.after ? " drop-before" : ""}` +
                `${dropHere && dropHere.after ? " drop-after" : ""}`
              }
              draggable
              onClick={() => navigate(`/sessions/${t.id}`)}
              onDoubleClick={() => renameSession(t.id, t.name)}
              // Middle-click closes the tab, like a browser/VS Code. mousedown
              // preventDefault stops the middle-click autoscroll circle; the
              // close fires on auxclick so a plain drag never triggers it.
              onMouseDown={(e) => {
                if (e.button === 1) e.preventDefault();
              }}
              onAuxClick={(e) => {
                if (e.button === 1) {
                  e.preventDefault();
                  closeTab(t.id);
                }
              }}
              onDragStart={(e) => {
                setDragId(t.id);
                e.dataTransfer.effectAllowed = "move";
                // Firefox requires data to be set for a drag to start at all.
                e.dataTransfer.setData("text/plain", t.id);
              }}
              onDragOver={(e) => {
                if (!dragId || dragId === t.id || !sameGroup) return;
                // Allow the drop and place the line on the near half.
                e.preventDefault();
                const r = e.currentTarget.getBoundingClientRect();
                const after = e.clientX > r.left + r.width / 2;
                if (dropAt?.id !== t.id || dropAt.after !== after) {
                  setDropAt({ id: t.id, after });
                }
              }}
              onDrop={(e) => {
                if (!dragId || dragId === t.id || !sameGroup) return;
                e.preventDefault();
                const r = e.currentTarget.getBoundingClientRect();
                const after = e.clientX > r.left + r.width / 2;
                store.reorder(dragId, t.id, after);
                endDrag();
              }}
              // Fires whether the drag ended in a drop or was released outside
              // the strip — so a cancelled drag leaves order and tabs untouched.
              onDragEnd={endDrag}
              title={`${t.name} · ${t.runtime}${st ? ` · ${st}` : ""}${
                agent ? ` · agent ${agent}` : ""
              }`}
            >
              {agent === "running" ? (
                <Loader2 size={12} className="session-tab-icon spin running" />
              ) : agent === "waiting" ? (
                <CircleDot size={12} className="session-tab-icon waiting" />
              ) : (
                <SquareTerminal
                  size={12}
                  className={`session-tab-icon ${dead ? "err" : "ok"}`}
                />
              )}
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
            </ContextMenuRegion>
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

    </>
  );
}
