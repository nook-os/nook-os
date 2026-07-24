// Shared control-plane switcher logic — the ONE implementation the dropdown
// pill (ControlPlanePill) and the always-visible tab strip (ControlPlaneTabs)
// both consume, so the two surfaces can never drift on how a switch, a rename,
// a forget, or a health probe behaves (MAIN-39).
//
// Desktop-only in effect: every action calls through `desktop.ts`, which no-ops
// in a browser, and both surfaces gate their render on `isDesktop()`.
import { useQuery, type QueryClient } from "@tanstack/react-query";
import { askText } from "./dialogs";
import {
  forgetControlPlane,
  listControlPlanes,
  probeControlPlane,
  renameControlPlane,
  setActiveControlPlane,
  type ControlPlane,
} from "./desktop";

/** The host part of a URL, for a tab/row label and the host subtitle. */
export function hostOf(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url.replace(/^https?:\/\//, "");
  }
}

// Reachability is cached ~30s and shared across BOTH surfaces (module-level), so
// the pill opening and the strip's interval do not each re-probe — one probe
// serves both within the window.
export const HEALTH_TTL = 30_000;
export const healthCache = new Map<string, { ok: boolean; at: number }>();

export type Health = "checking" | "up" | "down";

export async function probeCached(url: string): Promise<boolean> {
  const hit = healthCache.get(url);
  if (hit && Date.now() - hit.at < HEALTH_TTL) return hit.ok;
  // Resolve within ~1s: an unreachable host would otherwise hang on the
  // browser's default fetch timeout and leave the dot spinning.
  const ok = await Promise.race([
    probeControlPlane(url).then((r) => r.ok),
    new Promise<boolean>((res) => setTimeout(() => res(false), 1000)),
  ]);
  healthCache.set(url, { ok, at: Date.now() });
  return ok;
}

/** The dot class + tooltip for a server's current health state. */
export function healthDot(state: Health | undefined): { cls: string; title: string } {
  const cls = state === "up" ? "up" : state === "down" ? "down" : "checking";
  const title =
    state === "up" ? "reachable" : state === "down" ? "unreachable" : "checking…";
  return { cls, title };
}

/** Probe a set of servers into a health map, honouring the shared cache. */
export function probeInto(
  servers: ControlPlane[],
  setHealth: (fn: (h: Record<string, Health>) => Record<string, Health>) => void,
  alive: () => boolean = () => true,
): void {
  for (const cp of servers) {
    const cached = healthCache.get(cp.base_url);
    if (cached && Date.now() - cached.at < HEALTH_TTL) {
      setHealth((h) => ({ ...h, [cp.base_url]: cached.ok ? "up" : "down" }));
      continue;
    }
    setHealth((h) => ({ ...h, [cp.base_url]: "checking" }));
    void probeCached(cp.base_url).then((ok) => {
      if (alive()) setHealth((h) => ({ ...h, [cp.base_url]: ok ? "up" : "down" }));
    });
  }
}

/** The store query both surfaces read — same list, same active server. */
export function useControlPlanes() {
  const { data: store } = useQuery({
    queryKey: ["control-planes"],
    queryFn: listControlPlanes,
    staleTime: 10_000,
  });
  const servers = store?.control_planes ?? [];
  const activeUrl = store?.active ?? null;
  const active = servers.find((c) => c.base_url === activeUrl) ?? servers[0];
  return { servers, activeUrl, active };
}

const defaultReload = () => window.location.reload();

/**
 * Switch to a stored server. A click on the ALREADY-ACTIVE server is a no-op
 * (returns false, touches nothing). Otherwise it sets active and reloads the
 * webview — the mechanism by which the app comes back up on the new server with
 * none of the previous one's data visible. `reload` is injectable for tests.
 */
export async function switchToControlPlane(
  url: string,
  activeUrl: string | null,
  reload: () => void = defaultReload,
): Promise<boolean> {
  if (url === activeUrl) return false;
  await setActiveControlPlane(url);
  reload();
  return true;
}

/** Rename via the shared text dialog; sets/clears the label (host still shown),
 *  and invalidates the store so both surfaces refresh. */
export async function renameControlPlaneWithDialog(
  cp: ControlPlane,
  qc: QueryClient,
): Promise<void> {
  const label = await askText({
    title: `Rename ${hostOf(cp.base_url)}`,
    description:
      "A display name for this control plane. Its host still shows underneath, " +
      "so a rename never hides which machine a tab points at.",
    label: "Shown as",
    value: cp.label ?? "",
    confirmLabel: "rename",
  });
  if (label === null) return;
  await renameControlPlane(cp.base_url, label);
  qc.invalidateQueries({ queryKey: ["control-planes"] });
}

/**
 * Forget a server and its token. Forgetting the ACTIVE server reloads — which
 * resolves to the first remaining server, or the Connect screen if none remain.
 * Forgetting a non-active server does not reload; the store is invalidated so
 * both surfaces drop the tab. `reload` is injectable for tests.
 */
export async function forgetControlPlaneAndReconcile(
  cp: ControlPlane,
  activeUrl: string | null,
  qc: QueryClient,
  reload: () => void = defaultReload,
): Promise<void> {
  const wasActive = cp.base_url === activeUrl;
  await forgetControlPlane(cp.base_url);
  if (wasActive) {
    reload();
    return;
  }
  qc.invalidateQueries({ queryKey: ["control-planes"] });
}
