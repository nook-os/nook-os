// The control-plane switcher — desktop only (NG-1). The web build is served by
// its own control plane and has nothing to switch between, so this whole
// component renders nothing there.
//
// It sits immediately left of the workspace switcher (AC-2). Opening it lists
// every stored server with the active one marked, each server's account and a
// reachability dot (AC-3, AC-9); choosing one makes it active and reloads the
// webview onto it (AC-4); "Add control plane…" and an expired token both drop
// to the Connect screen (AC-5, AC-6); right-click renames or forgets (AC-7).
import React, { useEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Check, Plus, Server, ChevronDown } from "lucide-react";
import { useAnchoredMenu } from "@nookos/ui";
import { isDesktop, type ControlPlane } from "./desktop";
import {
  forgetControlPlaneAndReconcile,
  healthCache,
  healthDot,
  HEALTH_TTL,
  hostOf,
  probeCached,
  renameControlPlaneWithDialog,
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
  const [ctx, setCtx] = useState<{ cp: ControlPlane; x: number; y: number } | null>(null);
  const [adding, setAdding] = useState<{ prefillUrl?: string; notice?: string } | null>(null);
  const ctxRef = useRef<HTMLDivElement>(null);

  const { servers, activeUrl, active } = useControlPlanes();

  const { hostRef, portal } = useAnchoredMenu(open, () => setOpen(false), {
    height: 320,
    matchWidth: false,
  });

  // Probe every server WHEN THE MENU OPENS, never while closed (AC-9).
  useEffect(() => {
    if (!open) return;
    let alive = true;
    for (const cp of servers) {
      const cached = healthCache.get(cp.base_url);
      if (cached && Date.now() - cached.at < HEALTH_TTL) {
        setHealth((h) => ({ ...h, [cp.base_url]: cached.ok ? "up" : "down" }));
        continue;
      }
      setHealth((h) => ({ ...h, [cp.base_url]: "checking" }));
      void probeCached(cp.base_url).then((ok) => {
        if (alive) setHealth((h) => ({ ...h, [cp.base_url]: ok ? "up" : "down" }));
      });
    }
    return () => {
      alive = false;
    };
    // servers identity changes with the query; base_urls are the real input.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, servers.map((s) => s.base_url).join(",")]);

  // Close the right-click menu on any outside click.
  useEffect(() => {
    if (!ctx) return;
    const away = (e: MouseEvent) => {
      if (!ctxRef.current?.contains(e.target as Node)) setCtx(null);
    };
    window.addEventListener("mousedown", away);
    return () => window.removeEventListener("mousedown", away);
  }, [ctx]);

  if (!active) return null; // configured servers always exist past first-run

  // Both surfaces delegate the actual switch/rename/forget to the shared module
  // (controlPlanes.ts); the pill only manages its own menu state around them.
  const switchTo = async (url: string) => {
    setOpen(false);
    await switchToControlPlane(url, activeUrl);
  };
  const rename = async (cp: ControlPlane) => {
    setCtx(null);
    await renameControlPlaneWithDialog(cp, qc);
  };
  const forget = async (cp: ControlPlane) => {
    setCtx(null);
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
        title={`control plane — ${hostOf(active.base_url)}`}
      >
        <Server size={13} />
        <span className="cp-pill-label">{active.label || hostOf(active.base_url)}</span>
        <ChevronDown size={12} />
      </button>

      {portal(
        <>
          {servers.map((cp) => {
            const isActive = cp.base_url === activeUrl;
            return (
              <button
                key={cp.base_url}
                className={`cp-row${isActive ? " current" : ""}`}
                onClick={() => switchTo(cp.base_url)}
                onContextMenu={(e) => {
                  e.preventDefault();
                  setOpen(false);
                  setCtx({ cp, x: e.clientX, y: e.clientY });
                }}
              >
                <span className="cp-row-check">{isActive && <Check size={13} />}</span>
                {dot(cp)}
                <span className="cp-row-text">
                  <span className="cp-row-name">
                    {cp.label || hostOf(cp.base_url)}
                  </span>
                  {/* When a custom label is set, the host shows underneath so a
                      rename never hides the machine (AC-3). */}
                  {cp.label && <span className="cp-row-host">{hostOf(cp.base_url)}</span>}
                  {cp.account && <span className="cp-row-account">{cp.account}</span>}
                </span>
              </button>
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

      {ctx && (
        <div
          ref={ctxRef}
          className="cp-context"
          style={{ position: "fixed", left: ctx.x, top: ctx.y }}
        >
          <button onClick={() => rename(ctx.cp)}>Rename…</button>
          <button onClick={() => forget(ctx.cp)}>Forget</button>
        </div>
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
