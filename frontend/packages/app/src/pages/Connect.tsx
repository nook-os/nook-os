import React, { useEffect, useRef, useState } from "react";
import {
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
export function Connect({ onDone }: { onDone: () => void }) {
  const [server, setServer] = useState("https://");
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [device, setDevice] = useState<DeviceStart | null>(null);
  const [showToken, setShowToken] = useState(false);
  const cancelled = useRef(false);

  useEffect(() => () => { cancelled.current = true; }, []);

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
    setDevice(start);

    // Poll here rather than in Rust so the code stays on screen and the window
    // stays responsive — a command that blocked until approval would freeze it
    // for up to ten minutes.
    const deadline = Date.now() + start.expires_in_secs * 1000;
    let wait = start.interval_secs * 1000;
    while (Date.now() < deadline && !cancelled.current) {
      await new Promise((r) => setTimeout(r, wait));
      try {
        const issued = await pollDeviceLogin(url, start);
        if (issued) {
          await saveDesktopEndpoint({ base_url: url, token: issued });
          setBusy(false);
          onDone();
          return;
        }
      } catch (e) {
        setBusy(false);
        setDevice(null);
        setError(e instanceof Error ? e.message : String(e));
        return;
      }
      // Providers ask us to back off by answering slow_down; widening every
      // round is simpler than tracking which answer we got and costs a
      // person nothing they would notice.
      wait += 1000;
    }
    setBusy(false);
    setDevice(null);
    setError("That code expired before it was approved. Try again.");
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
          <p className="muted small login-claim">Waiting for approval…</p>
          <button
            className="btn"
            onClick={() => {
              cancelled.current = true;
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
      </div>
    </div>
  );
}
