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
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Check, Plus, Server, ChevronDown } from "lucide-react";
import { useAnchoredMenu } from "@nookos/ui";
import {
  forgetControlPlane,
  isDesktop,
  listControlPlanes,
  probeControlPlane,
  renameControlPlane,
  setActiveControlPlane,
  type ControlPlane,
} from "./desktop";
import { askText } from "./dialogs";
import { Connect } from "./pages/Connect";

/** The host part of a URL, for the pill label and the row subtitle. */
function hostOf(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url.replace(/^https?:\/\//, "");
  }
}

// Reachability is cached ~30s so reopening the menu does not re-probe (AC-9),
// and nothing is probed while the menu is closed. Module-level so the cache
// survives the menu unmounting.
const HEALTH_TTL = 30_000;
const healthCache = new Map<string, { ok: boolean; at: number }>();

async function probeCached(url: string): Promise<boolean> {
  const hit = healthCache.get(url);
  if (hit && Date.now() - hit.at < HEALTH_TTL) return hit.ok;
  // Resolve within ~1s (AC-9): an unreachable host would otherwise hang on the
  // browser's default fetch timeout and leave the dot spinning.
  const ok = await Promise.race([
    probeControlPlane(url).then((r) => r.ok),
    new Promise<boolean>((res) => setTimeout(() => res(false), 1000)),
  ]);
  healthCache.set(url, { ok, at: Date.now() });
  return ok;
}

type Health = "checking" | "up" | "down";

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

  const { data: store } = useQuery({
    queryKey: ["control-planes"],
    queryFn: listControlPlanes,
    staleTime: 10_000,
  });
  const servers = store?.control_planes ?? [];
  const activeUrl = store?.active ?? null;
  const active = servers.find((c) => c.base_url === activeUrl) ?? servers[0];

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

  const switchTo = async (url: string) => {
    setOpen(false);
    if (url === activeUrl) return;
    await setActiveControlPlane(url);
    // The reload is the mechanism (NG-5): the app comes back up on the new
    // server, with none of the previous one's data visible (AC-4).
    window.location.reload();
  };

  const rename = async (cp: ControlPlane) => {
    setCtx(null);
    const label = await askText({
      title: `Rename ${hostOf(cp.base_url)}`,
      description:
        "A display name for this control plane. Its host still shows underneath, " +
        "so a rename never hides which machine a row points at.",
      label: "Shown as",
      value: cp.label ?? "",
      confirmLabel: "rename",
    });
    if (label === null) return;
    await renameControlPlane(cp.base_url, label);
    qc.invalidateQueries({ queryKey: ["control-planes"] });
  };

  const forget = async (cp: ControlPlane) => {
    setCtx(null);
    const wasActive = cp.base_url === activeUrl;
    await forgetControlPlane(cp.base_url);
    if (wasActive) {
      // Switched to the first remaining server, or the Connect screen if none
      // remain — a reload resolves to whichever (AC-7).
      window.location.reload();
      return;
    }
    qc.invalidateQueries({ queryKey: ["control-planes"] });
  };

  const dot = (cp: ControlPlane) => {
    const s = health[cp.base_url];
    const cls = s === "up" ? "up" : s === "down" ? "down" : "checking";
    const title =
      s === "up" ? "reachable" : s === "down" ? "unreachable" : "checking…";
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
