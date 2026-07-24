import React, { useEffect, useRef, useState } from "react";
import {
  openExternal,
  probeControlPlane,
  probeToken,
  saveDesktopEndpoint,
  startDeviceLogin,
  pollDeviceLogin,
  type DeviceStart,
} from "../desktop";

/**
 * First run in the desktop app: which control plane, and who are you.
 *
 * Signing in goes through the identity provider, because requiring a pasted
 * token meant first-run was "open a browser, find the web UI, mint a token,
 * copy it here" — and on an instance nobody browses, there was no way in at
 * all. Pasting a token stays as the fallback for instances with no provider.
 */
/** A device authorization in progress, persisted so it survives the window
 *  closing. Approval happens in a browser, and a person naturally alt-tabs or
 *  closes the app while doing it — losing the flow to React state stranded them
 *  at the URL screen with an approval nobody was waiting for. */
interface Pending {
  url: string;
  start: DeviceStart;
  /** Epoch ms after which the code is dead and the flow should be dropped. */
  deadline: number;
}

const PENDING_KEY = "nookos-pending-device-login";

function loadPending(): Pending | null {
  try {
    const p = JSON.parse(localStorage.getItem(PENDING_KEY) ?? "null") as Pending | null;
    if (!p?.start?.device_code || !p.url || !(p.deadline > Date.now())) return null;
    return p;
  } catch {
    return null;
  }
}
function savePending(p: Pending) {
  try {
    localStorage.setItem(PENDING_KEY, JSON.stringify(p));
  } catch {
    // storage unavailable — the flow still works within this window, it just
    // won't survive a reopen. Nothing to do but carry on.
  }
}
function clearPending() {
  try {
    localStorage.removeItem(PENDING_KEY);
  } catch {
    // ignore
  }
}

export function Connect({
  onDone,
  prefillUrl,
  notice,
  onCancel,
}: {
  onDone: () => void;
  /** Start with this server filled in (re-connecting an expired one, or adding). */
  prefillUrl?: string;
  /** A line shown above the form — e.g. why a re-connect is being asked for. */
  notice?: string;
  /** When present, a way to back out (the "add another" overlay has somewhere
   *  to return to; first-run does not). */
  onCancel?: () => void;
}) {
  const [server, setServer] = useState(prefillUrl || "https://");
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [device, setDevice] = useState<DeviceStart | null>(null);
  const [showToken, setShowToken] = useState(false);
  // The device_code of the poll loop that should be running, or null for none.
  // A single ref, so a resumed flow supersedes any older loop instead of the
  // two racing — the closure checks `active === myCode` every round.
  const active = useRef<string | null>(null);

  /** Poll the provider until approval, expiry, or supersession. Reusable by
   *  the fresh sign-in and by the resume-on-reopen path. */
  const poll = async (url: string, start: DeviceStart, deadline: number) => {
    const myCode = start.device_code;
    active.current = myCode;
    setDevice(start);
    setBusy(true);
    setError(null);

    let wait = start.interval_secs * 1000;
    while (Date.now() < deadline && active.current === myCode) {
      await new Promise((r) => setTimeout(r, wait));
      if (active.current !== myCode) return; // superseded or cancelled
      try {
        const issued = await pollDeviceLogin(url, start);
        if (issued) {
          clearPending();
          active.current = null;
          await saveDesktopEndpoint({ base_url: url, token: issued });
          setBusy(false);
          onDone();
          return;
        }
      } catch (e) {
        clearPending();
        active.current = null;
        setBusy(false);
        setDevice(null);
        setError(e instanceof Error ? e.message : String(e));
        return;
      }
      // Providers ask us to back off by answering slow_down; widening every
      // round is simpler than tracking which answer we got and costs a person
      // nothing they would notice.
      wait += 1000;
    }
    if (active.current !== myCode) return; // a newer flow owns the screen now
    clearPending();
    active.current = null;
    setBusy(false);
    setDevice(null);
    setError("That code expired before it was approved. Try again.");
  };

  // Reopened mid-approval? Pick the flow back up and keep polling, rather than
  // showing the URL screen as if nothing had happened. This is the whole point
  // of persisting it: approval you already gave completes on its own.
  useEffect(() => {
    const p = loadPending();
    if (p) void poll(p.url, p.start, p.deadline);
    // Stop polling when this screen goes away; the persisted flow lets a later
    // mount resume, so this is a pause, not a cancel.
    return () => {
      active.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** Shared by both paths: the address has to be usable before anything else. */
  const checkedUrl = async (): Promise<string | null> => {
    const url = server.trim().replace(/\/+$/, "");
    // 8081 is the agent port — mutual TLS, for nodes. It serves /healthz, so a
    // naive check passes and everything after it fails.
    if (/:8081(\/|$)/.test(url)) {
      setError(
        "That is the agent port, which only nodes use. Use the same address " +
          "you open in a browser — usually without a port.",
      );
      return null;
    }
    const reachable = await probeControlPlane(url);
    if (!reachable.ok) {
      setError(reachable.detail);
      return null;
    }
    return url;
  };

  const signIn = async () => {
    setError(null);
    setBusy(true);
    const url = await checkedUrl();
    if (!url) return setBusy(false);

    let start: DeviceStart;
    try {
      start = await startDeviceLogin(url);
    } catch (e) {
      setBusy(false);
      setError(e instanceof Error ? e.message : String(e));
      return;
    }

    // Persist BEFORE polling, so an approval given after the window is closed
    // is still claimed on the next open. Poll on the JS side (not a blocking
    // Rust command) so the code stays on screen and the window stays live.
    const deadline = Date.now() + start.expires_in_secs * 1000;
    savePending({ url, start, deadline });
    // "Your browser should have opened" was a claim nobody was making true:
    // nothing opened it, and the fallback link navigated this webview to the
    // provider — an origin where `device_poll` is denied, so the flow it was
    // offering to complete could never complete. Open it for real.
    void openExternal(start.verification_uri);
    await poll(url, start, deadline);
  };

  const useToken = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    setBusy(true);
    const url = await checkedUrl();
    if (!url) return setBusy(false);

    const accepted = await probeToken(url, token.trim());
    if (!accepted.ok) {
      setBusy(false);
      setError(accepted.detail);
      return;
    }
    await saveDesktopEndpoint({ base_url: url, token: token.trim() });
    setBusy(false);
    onDone();
  };

  if (device) {
    return (
      <div className="login-screen">
        <div className="login-box">
          <div className="login-title">◆ nook@os</div>
          <div className="muted small">approve this computer</div>
          <p className="muted small login-claim">
            Your browser should have opened. If not, go to:
          </p>
          <a className="device-link" href={device.verification_uri}>
            {device.verification_uri}
          </a>
          <div className="device-code">{device.user_code}</div>
          <p className="muted small login-claim">
            Waiting for approval… once you approve in the browser, this window
            signs you in on its own — you can leave it open or come back to it.
          </p>
          <button
            className="btn"
            onClick={() => {
              active.current = null;
              clearPending();
              setDevice(null);
              setBusy(false);
            }}
          >
            Cancel
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className="login-screen">
      <div className="login-box">
        <div className="login-title">◆ nook@os</div>
        <div className="muted small">connect to a control plane</div>

        {notice && (
          <div className="small login-notice" role="status">
            {notice}
          </div>
        )}

        <form className="login-form" onSubmit={(e) => { e.preventDefault(); signIn(); }}>
          <label className="login-field">
            <span className="small muted">Control plane URL</span>
            <input
              value={server}
              onChange={(e) => setServer(e.target.value)}
              placeholder="https://nook.example.com"
              autoFocus
              required
            />
          </label>

          {showToken && (
            <label className="login-field">
              <span className="small muted">User token</span>
              <input
                type="password"
                value={token}
                onChange={(e) => setToken(e.target.value)}
                placeholder="nook_user_…"
                autoComplete="off"
              />
            </label>
          )}

          {error && (
            <div className="small login-error" role="alert">
              {error}
            </div>
          )}

          {showToken ? (
            <button className="btn primary" onClick={useToken} disabled={busy}>
              {busy ? "Checking…" : "Connect with token"}
            </button>
          ) : (
            <button className="btn primary" type="submit" disabled={busy}>
              {busy ? "Starting…" : "Sign in"}
            </button>
          )}
        </form>

        <button
          className="btn small"
          onClick={() => {
            setShowToken((v) => !v);
            setError(null);
          }}
        >
          {showToken
            ? "Sign in with your identity provider instead"
            : "Use a token instead"}
        </button>

        {onCancel && (
          <button className="btn small" onClick={onCancel} disabled={busy}>
            Cancel
          </button>
        )}
      </div>
    </div>
  );
}
