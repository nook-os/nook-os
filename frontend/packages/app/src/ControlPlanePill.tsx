// The control-plane switcher — desktop only (NG-1). The web build is served by
// its own control plane and has nothing to switch between, so this whole
// component renders nothing there.
//
// It sits immediately left of the workspace switcher (AC-2). Opening it lists
// every stored server with the active one marked, each server's account and a
// reachability dot (AC-3, AC-9); choosing one makes it active and reloads the
// webview onto it (AC-4); "Add control plane…" and an expired token both drop
// to the Connect screen (AC-5, AC-6); right-click renames or forgets (AC-7).
import React, { useEffect, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Check, Plus, Server, ChevronDown } from "lucide-react";
import { ContextMenuRegion } from "./contextMenu";
import { useAnchoredMenu } from "@nookos/ui";
import { isDesktop, isLocalPlane, type ControlPlane } from "./desktop";
import {
  displayName,
  forgetControlPlaneAndReconcile,
  healthDot,
  probeInto,
  renameControlPlaneWithDialog,
  subtitleOf,
  switchToControlPlane,
  useControlPlanes,
  type Health,
} from "./controlPlanes";
import { Connect } from "./pages/Connect";

export function ControlPlanePill() {
  // Stable per environment, so an early return before the hooks below does not
  // violate the rules of hooks — the desktop build always takes one branch, the
  // web build always the other.
  if (!isDesktop()) return null;
  return <Pill />;
}

function Pill() {
  const qc = useQueryClient();
  const [open, setOpen] = useState(false);
  const [health, setHealth] = useState<Record<string, Health>>({});
  const [adding, setAdding] = useState<{ prefillUrl?: string; notice?: string } | null>(null);

  const { servers, activeUrl, active } = useControlPlanes();

  const { hostRef, portal } = useAnchoredMenu(open, () => setOpen(false), {
    height: 320,
    matchWidth: false,
  });

  // Probe every server WHEN THE MENU OPENS, never while closed (AC-9).
  useEffect(() => {
    if (!open) return;
    let alive = true;
    probeInto(servers, setHealth, () => alive);
    return () => {
      alive = false;
    };
    // servers identity changes with the query; base_urls are the real input.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, servers.map((s) => s.base_url).join(",")]);

  if (!active) return null; // configured servers always exist past first-run

  // Both surfaces delegate the actual switch/rename/forget to the shared module
  // (controlPlanes.ts); the pill only manages its own menu state around them.
  const switchTo = async (url: string) => {
    setOpen(false);
    await switchToControlPlane(url, activeUrl);
  };
  const rename = async (cp: ControlPlane) => {
    setOpen(false);
    await renameControlPlaneWithDialog(cp, qc);
  };
  const forget = async (cp: ControlPlane) => {
    setOpen(false);
    await forgetControlPlaneAndReconcile(cp, activeUrl, qc);
  };

  const dot = (cp: ControlPlane) => {
    const { cls, title } = healthDot(health[cp.base_url]);
    return <span className={`cp-dot ${cls}`} title={title} />;
  };

  return (
    <div className="cp-pill-wrap" ref={hostRef}>
      <button
        className="cp-pill"
        onClick={() => setOpen((o) => !o)}
        title={`control plane — ${subtitleOf(active)}`}
      >
        <Server size={13} />
        <span className="cp-pill-label">{displayName(active)}</span>
        <ChevronDown size={12} />
      </button>

      {portal(
        <>
          {servers.map((cp) => {
            const isActive = cp.base_url === activeUrl;
            const local = isLocalPlane(cp);
            const row = (
              <button
                className={`cp-row${isActive ? " current" : ""}`}
                onClick={() => switchTo(cp.base_url)}
                title={subtitleOf(cp)}
              >
                <span className="cp-row-check">{isActive && <Check size={13} />}</span>
                {dot(cp)}
                <span className="cp-row-text">
                  <span className="cp-row-name">{displayName(cp)}</span>
                  {/* When a custom label is set, the host shows underneath so a
                      rename never hides the machine (AC-3). Local has no host to
                      show and is not renamed, so it says what it is instead. */}
                  {(local || cp.label) && (
                    <span className="cp-row-host">{subtitleOf(cp)}</span>
                  )}
                  {cp.account && <span className="cp-row-account">{cp.account}</span>}
                </span>
              </button>
            );
            // Rename and Forget are both edits to a stored ADDRESS, and Local is
            // not one: there is no URL to relabel, and forgetting it would
            // discard the only credential to a database still sitting on disk
            // (AC-3/AC-4). So its row carries no manage menu at all.
            return local ? (
              <React.Fragment key={cp.base_url}>{row}</React.Fragment>
            ) : (
              // Right-click a server row → rename/forget, via the shared menu
              // (MAIN-168). `display: contents` keeps the dropdown layout intact.
              <ContextMenuRegion
                key={cp.base_url}
                style={{ display: "contents" }}
                items={() => [
                  { label: "Rename…", onSelect: () => void rename(cp) },
                  { label: "Forget", onSelect: () => void forget(cp) },
                ]}
              >
                {row}
              </ContextMenuRegion>
            );
          })}
          <button
            className="cp-row cp-row-add"
            onClick={() => {
              setOpen(false);
              setAdding({});
            }}
          >
            <span className="cp-row-check" />
            <Plus size={13} />
            <span className="cp-row-text">
              <span className="cp-row-name">Add control plane…</span>
            </span>
          </button>
        </>,
        "cp-menu",
      )}

      {adding && (
        <div className="cp-add-overlay">
          <Connect
            prefillUrl={adding.prefillUrl}
            notice={adding.notice}
            onCancel={() => setAdding(null)}
            onDone={() => {
              // A new (or re-authenticated) server is now active — reload onto
              // it, same as switching (AC-4, AC-5).
              window.location.reload();
            }}
          />
        </div>
      )}
    </div>
  );
}
