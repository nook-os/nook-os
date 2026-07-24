// Desktop-only wiring: which control plane, and which credential.
//
// The web build answers both by being served from the control plane. A
// packaged app is served from `tauri://localhost`, so it has to be told — and
// told once, persistently, or every launch is a setup wizard.
//
// Everything here no-ops in a browser, so the same bundle serves both.

import { setEndpoint } from "@nookos/api";

export interface DesktopEndpoint {
  base_url: string;
  token: string;
}

/** One stored control plane. `base_url` is the identity (one entry per URL). */
export interface ControlPlane {
  base_url: string;
  token: string;
  label?: string | null;
  account?: string | null;
}

/** The desktop store: every control plane and which is active (by base_url). */
export interface ControlPlaneStore {
  control_planes: ControlPlane[];
  active: string | null;
}

// The active server's URL, cached synchronously in localStorage so modules that
// load before any async call (the session-tabs store) can namespace by it
// without awaiting. Rewritten on every initDesktop / switch; the webview reload
// that a switch triggers is what makes those modules pick up the new value.
const ACTIVE_CP_KEY = "nook.active-cp";

/** The active control plane's URL, or "" on the web build. Synchronous. */
export function activeControlPlaneKey(): string {
  try {
    return localStorage.getItem(ACTIVE_CP_KEY) ?? "";
  } catch {
    return "";
  }
}

function rememberActive(url: string) {
  try {
    if (url) localStorage.setItem(ACTIVE_CP_KEY, url);
    else localStorage.removeItem(ACTIVE_CP_KEY);
  } catch {
    // storage unavailable — tab namespacing falls back to the shared key
  }
}

/** True when running inside the Tauri shell rather than a browser tab. */
export function isDesktop(): boolean {
  return typeof (window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ !==
    "undefined";
}

type Invoke = <T>(cmd: string, args?: Record<string, unknown>) => Promise<T>;

function invoke(): Invoke | null {
  const internals = (
    window as { __TAURI_INTERNALS__?: { invoke?: Invoke } }
  ).__TAURI_INTERNALS__;
  return internals?.invoke ?? null;
}

/**
 * Load the stored endpoint and point the API client at it.
 *
 * Returns the endpoint so the caller can decide whether to show the connect
 * screen: an empty `base_url` means nobody has configured this install yet.
 */
export async function initDesktop(): Promise<DesktopEndpoint | null> {
  if (!isDesktop()) return null;
  const call = invoke();
  if (!call) return null;

  const stored = await call<DesktopEndpoint>("load_endpoint");
  if (stored.base_url) {
    setEndpoint({ baseUrl: stored.base_url, token: stored.token });
  }
  rememberActive(stored.base_url);
  return stored;
}

/**
 * Add a control plane (or re-authenticate one already stored, replacing its
 * token in place) and make it active. Used by the Connect screen for both
 * first-run and "Add control plane…".
 */
export async function saveDesktopEndpoint(
  endpoint: DesktopEndpoint,
): Promise<void> {
  const call = invoke();
  if (!call) return;
  await call("add_control_plane", { endpoint });
  setEndpoint({ baseUrl: endpoint.base_url, token: endpoint.token });
  rememberActive(endpoint.base_url);
}

/** Every stored control plane and which is active (empty on the web build). */
export async function listControlPlanes(): Promise<ControlPlaneStore> {
  const call = invoke();
  if (!call) return { control_planes: [], active: null };
  return call<ControlPlaneStore>("list_control_planes");
}

/**
 * Make a stored server active. The caller reloads the webview afterwards
 * (AC-4): the whole app — /auth/me, the live socket, board, workspaces — must
 * come back up on the new server with nothing from the old one visible, and a
 * reload is the mechanism (NG-5).
 */
export async function setActiveControlPlane(url: string): Promise<void> {
  const call = invoke();
  if (!call) return;
  await call("set_active_control_plane", { url });
  rememberActive(url);
}

/** Remove a server and its token from disk. */
export async function forgetControlPlane(url: string): Promise<void> {
  const call = invoke();
  if (!call) return;
  await call("forget_control_plane", { url });
  // Its namespaced session tabs go too — nothing left points at that server.
  try {
    localStorage.removeItem(sessionTabsKey(url));
  } catch {
    // ignore
  }
}

/** Set (or clear, with "") a server's custom label. */
export async function renameControlPlane(
  url: string,
  label: string,
): Promise<void> {
  const call = invoke();
  if (!call) return;
  await call("rename_control_plane", { url, label });
}

/** Record which account is signed in on a server (backfilled from /auth/me). */
export async function setControlPlaneAccount(
  url: string,
  account: string,
): Promise<void> {
  const call = invoke();
  if (!call) return;
  await call("set_control_plane_account", { url, account });
}

/** localStorage key holding the session tabs for one control plane (AC-8). The
 *  web build (empty key) keeps the original un-namespaced key for continuity. */
export function sessionTabsKey(cpKey: string): string {
  return cpKey ? `nook.session-tabs::${cpKey}` : "nook.session-tabs";
}

/**
 * Hand a URL to the OS browser.
 *
 * Never `window.open`, and never a plain `<a href>`, in the packaged app: both
 * move the ONE webview off `tauri://localhost`. On any other origin Tauri
 * refuses every command in this file by ACL, so the app arrives somewhere it
 * cannot read its own endpoint and reports itself as unconfigured. Opening
 * elsewhere is the only safe thing to do with an address that is not ours.
 */
export async function openExternal(url: string): Promise<void> {
  const call = invoke();
  if (!call) {
    // A browser tab has more than one of itself; leaving is free.
    window.open(url, "_blank", "noopener");
    return;
  }
  await call("open_external", { url });
}

/**
 * Check a control plane answers before we store its address.
 *
 * `/healthz` needs no credential, so this separates "wrong address" from
 * "wrong token" — two failures that otherwise arrive as one confusing screen.
 */
export async function probeControlPlane(
  baseUrl: string,
): Promise<{ ok: boolean; detail: string }> {
  const url = baseUrl.replace(/\/+$/, "");
  try {
    const res = await fetch(`${url}/healthz`, { method: "GET" });
    if (!res.ok) return { ok: false, detail: `answered ${res.status}` };
    return { ok: true, detail: "" };
  } catch (e) {
    // A blocked cross-origin request and an unreachable host are the same
    // TypeError here — the browser refuses to say which, deliberately. Naming
    // only the second sent someone chasing DNS for a server that was answering
    // perfectly well, so name both.
    const why = e instanceof Error ? e.message : String(e);
    return {
      ok: false,
      detail:
        `Could not read a response from ${url}. Either it is unreachable, or ` +
        `it is running a version from before desktop support and is refusing ` +
        `the request. Opening ${url}/healthz in a browser tells you which: if ` +
        `that works, the control plane needs updating. (${why})`,
    };
  }
}

/** Confirm a token is accepted, so a bad paste fails here rather than later. */
export async function probeToken(
  baseUrl: string,
  token: string,
): Promise<{ ok: boolean; detail: string }> {
  const url = baseUrl.replace(/\/+$/, "");
  try {
    const res = await fetch(`${url}/api/v1/auth/me`, {
      headers: { Authorization: `Bearer ${token}` },
    });
    if (res.status === 401 || res.status === 403) {
      return { ok: false, detail: "that token was rejected" };
    }
    if (!res.ok) return { ok: false, detail: `answered ${res.status}` };
    return { ok: true, detail: "" };
  } catch (e) {
    return {
      ok: false,
      detail: e instanceof Error ? e.message : "could not reach it",
    };
  }
}

/** What the person must approve, and what `pollDeviceLogin` needs to continue. */
export interface DeviceStart {
  user_code: string;
  verification_uri: string;
  device_code: string;
  token_endpoint: string;
  client_id: string;
  interval_secs: number;
  expires_in_secs: number;
}

/**
 * Ask the identity provider to start a device authorization.
 *
 * Runs in Rust, not here: a request from `tauri://localhost` to the provider is
 * cross-origin, and no provider is going to add CORS for a desktop app's
 * private scheme.
 */
export async function startDeviceLogin(server: string): Promise<DeviceStart> {
  const call = invoke();
  if (!call) throw new Error("not running in the desktop app");
  return call<DeviceStart>("device_start", { server });
}

/** One poll. `null` means nobody has approved it yet. */
export async function pollDeviceLogin(
  server: string,
  start: DeviceStart,
): Promise<string | null> {
  const call = invoke();
  if (!call) throw new Error("not running in the desktop app");
  return call<string | null>("device_poll", { server, start });
}

export interface AvailableUpdate {
  version: string;
  current: string;
  notes: string;
}

/** Is a newer desktop release out? `null` when current, or not the app. */
export async function checkForUpdate(): Promise<AvailableUpdate | null> {
  const call = invoke();
  if (!call) return null;
  try {
    return await call<AvailableUpdate | null>("update_check");
  } catch {
    // Being offline, or GitHub being unreachable, is not worth interrupting
    // anyone over — the check runs again next launch.
    return null;
  }
}

/** Download, verify, install, restart. Does not return on success. */
export async function installUpdate(): Promise<void> {
  const call = invoke();
  if (!call) return;
  await call("update_install");
}
