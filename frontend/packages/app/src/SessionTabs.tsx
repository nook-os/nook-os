// The tab strip above the terminal — VS-Code-style: click to switch, × to
// close the tab (the session keeps running), + to start new work.
import React from "react";
import { useNavigate } from "react-router-dom";
import { Plus, SquareTerminal, X } from "lucide-react";
import { useWorkspaceContext } from "./context";
import { useLive } from "./live";
import { useNewWork } from "./newwork";
import { useSessionTabs } from "./sessiontabs";

export function SessionTabs({ activeId }: { activeId?: string }) {
  const navigate = useNavigate();
  const allTabs = useSessionTabs((s) => s.tabs);
  const close = useSessionTabs((s) => s.close);
  const sessionStatus = useLive((s) => s.sessionStatus);
  const showNewWork = useNewWork((s) => s.show);
  const selectedWorkspaceId = useWorkspaceContext((s) => s.selectedWorkspaceId);

  // Tabs are scoped to the workspace context; "all workspaces" shows every
  // tab, labeled with its workspace so cross-workspace tabs stay tellable.
  const tabs = selectedWorkspaceId
    ? allTabs.filter((t) => !t.workspaceId || t.workspaceId === selectedWorkspaceId)
    : allTabs;

  if (tabs.length === 0) return null;

  const closeTab = (e: React.MouseEvent, id: string) => {
    e.stopPropagation();
    const idx = tabs.findIndex((t) => t.id === id);
    close(id);
    if (id === activeId) {
      // Next stop comes from the VISIBLE (filtered) strip.
      const next = tabs[idx + 1] ?? tabs[idx - 1];
      navigate(next && next.id !== id ? `/sessions/${next.id}` : "/sessions");
    }
  };

  return (
    <div className="session-tabs">
      {tabs.map((t) => {
        const st = sessionStatus[t.id];
        const dead = st === "exited" || st === "error" || st === "killed";
        return (
          <div
            key={t.id}
            className={`session-tab${t.id === activeId ? " active" : ""}`}
            onClick={() => navigate(`/sessions/${t.id}`)}
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
            <button
              className="session-tab-close"
              title="close tab (session keeps running)"
              onClick={(e) => closeTab(e, t.id)}
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
  );
}
