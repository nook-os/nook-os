// The tab strip above the terminal — click to switch, right-click for the rest,
// + to start new work.
//
// MAIN-322: the strip is a VIEW of the live session list, not a per-browser
// open-set. Every machine signed into the same account shows the same tabs,
// because the tabs are the sessions. There is deliberately no close control
// here: closing used to mean "drop this from my local list", and with no local
// list left, closing can only mean ending the session — which is destructive
// and differs for managed vs ad-hoc sessions, so MAIN-324 owns it. Until then a
// session is ended from the session view's kill control or the sessions list,
// and its tab disappears on its own because the tab was only ever the session.
import React, { useState } from "react";
import { useNavigate } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import {
  ChevronDown,
  ChevronRight,
  CircleDot,
  Loader2,
  Pin,
  Plus,
  Server,
  SquareTerminal,
} from "lucide-react";
import { api } from "@nookos/api";
import { useWorkspaceContext } from "./context";
import { useLive } from "./live";
import { useLiveTabs } from "./liveTabs";
import { useNewWork } from "./newwork";
import { useSessionTabPrefs } from "./sessionTabsStore";
import { groupTabs, visibleTabs } from "./tabGroups";
import { useTabHotkeys } from "./tabHotkeys";
import { askText, notify } from "./dialogs";
import { ContextMenuRegion, type ContextMenuItem } from "./contextMenu";

export function SessionTabs({ activeId }: { activeId?: string }) {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const store = useSessionTabPrefs();
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

  // The tab set — every live session of yours, in your order (MAIN-321
  // moved this to a hook the `/sessions` redirect shares, so "first tab" and
  // "the session the nav opens" cannot disagree).
  const { tabs } = useLiveTabs();

  // Chrome-style groups by workspace (MAIN-323), each collapsible.
  const groups = groupTabs(tabs, store.prefs.collapsed, activeId);
  // What is actually on screen. Keyboard switching walks exactly this — landing
  // on a tab inside a collapsed group would move the terminal to a session the
  // strip is not showing.
  const visible = visibleTabs(groups);

  // Chrome-style Ctrl+Tab / Ctrl+Cmd-number switching over exactly this visible
  // list (desktop only). Called before the early return so the hook order is
  // stable across renders; with an empty list it simply has nothing to switch.
  useTabHotkeys(
    visible.map((t) => t.id),
    activeId,
    navigate,
  );

  if (tabs.length === 0) return null;

  /** Rename the session itself, so every viewer sees it — not just this tab.
   *  With the strip sourced from the session list there is nothing local to
   *  rename: the PATCH plus a refetch IS the rename. */
  const renameSession = async (id: string, current: string) => {
    const name = await askText({
      title: "Rename session",
      label: "Session name",
      value: current,
      confirmLabel: "rename",
    });
    if (!name || name === current) return;
    const { error } = await api.PATCH("/api/v1/sessions/{id}", {
      params: { path: { id } },
      body: { name },
    });
    if (error) {
      await notify("Rename failed", "The control plane rejected the change.");
      return;
    }
    queryClient.invalidateQueries();
  };

  // The tab's right-click menu, as items for the shared primitive (MAIN-168).
  const tabMenu = (t: (typeof tabs)[number]): ContextMenuItem[] => [
    { label: t.pinned ? "Unpin" : "Pin", onSelect: () => store.togglePin(t.id) },
    { label: "Rename Session…", onSelect: () => renameSession(t.id, t.name) },
    { separator: true },
    {
      label: "Copy Session ID",
      onSelect: () => void navigator.clipboard?.writeText(t.id).catch(() => {}),
    },
  ];

  return (
    <>
      <div className="session-tabs">
        {groups.map((g) => (
          <React.Fragment key={g.key}>
            {/* The group chip: name, count, and the collapse toggle. Coloured
                from the workspace id, so the same repo is the same colour on
                every machine with no palette to keep in sync. */}
            <button
              className={`tab-group-chip${g.collapsed ? " collapsed" : ""}`}
              style={
                {
                  "--group-hue": g.hue,
                } as React.CSSProperties
              }
              onClick={() => store.toggleCollapsed(g.key)}
              title={
                g.collapsed
                  ? `expand ${g.label} (${g.tabs.length})`
                  : `collapse ${g.label}`
              }
            >
              {g.collapsed ? <ChevronRight size={11} /> : <ChevronDown size={11} />}
              <span className="tab-group-name">{g.label}</span>
              <span className="tab-group-count">{g.tabs.length}</span>
            </button>
            {!g.collapsed &&
              g.tabs.map((t) => {
          const st = sessionStatus[t.id];
          const dead = st === "exited" || st === "error" || st === "killed";
          const dragged = dragId ? visible.find((x) => x.id === dragId) : null;
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
                store.reorder(dragId, t.id, after, visible);
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
              {/* The group chip already names the workspace, so the tab
                  names the MACHINE instead — which is the thing that tells
                  four VMs of one repo apart (AC-2). */}
              {t.nodeName && (
                <span className="session-tab-node" title={`machine: ${t.nodeName}`}>
                  <Server size={9} />
                  {t.nodeName}
                </span>
              )}
              <span className="session-tab-name">{t.name}</span>
              {t.pinned && <Pin size={10} className="session-tab-pin" />}
            </div>
            </ContextMenuRegion>
          );
              })}
          </React.Fragment>
        ))}
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
