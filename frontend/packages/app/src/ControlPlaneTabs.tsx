// An always-visible, VSCode-style row of control-plane tabs across the very top
// of the desktop window — click a tab to jump to that server (MAIN-39). The
// dropdown pill stays (AC-6); both read the same store and share every action
// via controlPlanes.ts, so they can never drift. Desktop-only (NG-1/NG-5): the
// web build renders nothing and its layout is unchanged.
import React, { useEffect, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Plus } from "lucide-react";
import { ContextMenuRegion } from "./contextMenu";
import { isDesktop } from "./desktop";
import {
  forgetControlPlaneAndReconcile,
  healthDot,
  hostOf,
  probeInto,
  renameControlPlaneWithDialog,
  switchToControlPlane,
  useControlPlanes,
  type Health,
} from "./controlPlanes";
import { Connect } from "./pages/Connect";

export function ControlPlaneTabs() {
  // Stable per environment (see ControlPlanePill) — the early return before the
  // hooks below is safe because isDesktop() never flips within a session.
  if (!isDesktop()) return null;
  return <Tabs />;
}

function Tabs() {
  const qc = useQueryClient();
  const { servers, activeUrl } = useControlPlanes();
  const [health, setHealth] = useState<Record<string, Health>>({});
  const [adding, setAdding] = useState(false);

  const serverKeys = servers.map((s) => s.base_url).join(",");

  // The strip is always on screen (unlike the on-open dropdown), so it probes on
  // mount and every ~30s WHILE VISIBLE, and re-probes the moment the app returns
  // to the foreground — but never while backgrounded/hidden (AC-3). The shared
  // 30s cache keeps a refocus from stampeding the servers.
  useEffect(() => {
    if (!servers.length) return;
    let alive = true;
    const probe = () => {
      if (!document.hidden) probeInto(servers, setHealth, () => alive);
    };
    probe();
    const id = setInterval(probe, 30_000);
    const onVisible = () => probe();
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      alive = false;
      clearInterval(id);
      document.removeEventListener("visibilitychange", onVisible);
    };
    // base_urls are the real input; the servers array identity changes per query.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [serverKeys]);

  // AC-1: the strip only appears once at least one control plane is configured.
  if (!servers.length) return null;

  return (
    <div className="cp-tabs" role="tablist" aria-label="Control planes">
      {servers.map((cp) => {
        const isActive = cp.base_url === activeUrl;
        const { cls, title } = healthDot(health[cp.base_url]);
        return (
          // Right-click a tab → rename/forget, via the shared menu (MAIN-168).
          <ContextMenuRegion
            key={cp.base_url}
            style={{ display: "contents" }}
            items={() => [
              {
                label: "Rename…",
                onSelect: () => void renameControlPlaneWithDialog(cp, qc),
              },
              {
                label: "Forget",
                onSelect: () =>
                  void forgetControlPlaneAndReconcile(cp, activeUrl, qc),
              },
            ]}
          >
            <button
              role="tab"
              aria-selected={isActive}
              className={`cp-tab${isActive ? " active" : ""}`}
              // The host rides in the tooltip so a renamed tab still says which
              // machine it points at, without a second line in this dense strip.
              title={hostOf(cp.base_url)}
              onClick={() => switchToControlPlane(cp.base_url, activeUrl)}
            >
              <span className={`cp-dot ${cls}`} title={title} />
              <span className="cp-tab-body">
                <span className="cp-tab-name">{cp.label || hostOf(cp.base_url)}</span>
                {cp.account && <span className="cp-tab-account">{cp.account}</span>}
              </span>
            </button>
          </ContextMenuRegion>
        );
      })}
      <button
        className="cp-tab-add"
        title="Add control plane…"
        onClick={() => setAdding(true)}
      >
        <Plus size={14} />
      </button>

      {adding && (
        <div className="cp-add-overlay">
          <Connect
            onCancel={() => setAdding(false)}
            onDone={() => window.location.reload()}
          />
        </div>
      )}
    </div>
  );
}
