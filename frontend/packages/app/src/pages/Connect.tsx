import React, { useState } from "react";
import {
  probeControlPlane,
  probeToken,
  saveDesktopEndpoint,
} from "../desktop";

/**
 * First run in the desktop app: which control plane, and which credential.
 *
 * A token rather than a username and password, because this client is not a
 * browser tab. The session cookie belongs to the control plane's origin, and a
 * custom `tauri://` scheme cannot hold one in any way that works the same on
 * macOS, Windows and Linux. A user token is a bearer credential built for
 * exactly this — it is what `nook login` already uses to drive the fleet.
 */
export function Connect({ onDone }: { onDone: () => void }) {
  const [server, setServer] = useState("https://");
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    setBusy(true);

    const url = server.trim().replace(/\/+$/, "");
    // Address first, then credential: separating the two means "wrong host"
    // and "wrong token" arrive as different sentences.
    const reachable = await probeControlPlane(url);
    if (!reachable.ok) {
      setBusy(false);
      setError(`Could not reach ${url} — ${reachable.detail}`);
      return;
    }

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

  return (
    <div className="login-screen">
      <div className="login-box">
        <div className="login-title">◆ nook@os</div>
        <div className="muted small">connect to a control plane</div>

        <form className="login-form" onSubmit={submit}>
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
          <label className="login-field">
            <span className="small muted">User token</span>
            <input
              type="password"
              value={token}
              onChange={(e) => setToken(e.target.value)}
              placeholder="nook_user_…"
              autoComplete="off"
              required
            />
          </label>
          <p className="muted small login-claim">
            Create one in the web UI under <b>Settings → Tokens</b>. It is stored
            on this machine only, readable by you alone.
          </p>
          {error && (
            <div className="small login-error" role="alert">
              {error}
            </div>
          )}
          <button className="btn primary" type="submit" disabled={busy}>
            {busy ? "Checking…" : "Connect"}
          </button>
        </form>
      </div>
    </div>
  );
}
