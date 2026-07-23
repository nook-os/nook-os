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
  return stored;
}

export async function saveDesktopEndpoint(
  endpoint: DesktopEndpoint,
): Promise<void> {
  const call = invoke();
  if (!call) return;
  await call("save_endpoint", { endpoint });
  setEndpoint({ baseUrl: endpoint.base_url, token: endpoint.token });
}

export async function clearDesktopEndpoint(): Promise<void> {
  const call = invoke();
  if (!call) return;
  await call("clear_endpoint");
  setEndpoint({ baseUrl: "", token: "" });
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
