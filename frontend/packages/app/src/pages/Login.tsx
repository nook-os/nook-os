import React, { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "@nookos/api";

/** Pull a readable message out of whatever the API returned. */
function messageOf(error: unknown, fallback: string): string {
  if (error && typeof error === "object") {
    const e = error as Record<string, unknown>;
    for (const key of ["message", "error", "detail"]) {
      if (typeof e[key] === "string" && e[key]) return e[key] as string;
    }
  }
  return fallback;
}

export function Login() {
  // Only offer sign-in methods this instance actually supports.
  const { data: providers } = useQuery({
    queryKey: ["auth", "providers"],
    queryFn: async () => (await api.GET("/api/v1/auth/providers")).data,
  });
  // Whether local sign-in is usable here, and whether anyone has claimed this
  // instance yet. An instance already committed to OIDC reports unavailable,
  // so the form is not offered where it could never work.
  const { data: local } = useQuery({
    queryKey: ["auth", "local", "status"],
    queryFn: async () => (await api.GET("/api/v1/auth/local/status")).data,
  });

  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const claiming = local?.needs_bootstrap === true;
  const showLocal = local?.available === true;

  // Dev sign-in as ANY account, the way Hearth does it: the "credential" is
  // the email you type. Testing an authorization model requires being
  // different people, and a model you cannot switch between users to exercise
  // is a model nobody exercises.
  const { data: devAccounts } = useQuery({
    queryKey: ["auth", "dev-accounts"],
    queryFn: async () => (await api.GET("/api/v1/auth/dev-accounts")).data ?? [],
    enabled: providers?.dev_login === true,
    // Refused outright when dev mode is off; that is an answer, not a problem
    // to keep retrying.
    retry: false,
  });
  const [devEmail, setDevEmail] = useState("");
  // The account list is a STEP, not the front page. "Sign in with Dev" sits
  // beside the other providers as one option among several; who you sign in as
  // is the question that comes after choosing it, the same way an IdP asks
  // after you have been redirected to it.
  const [devOpen, setDevOpen] = useState(false);

  const devLogin = async (email?: string) => {
    setError(null);
    const { error, response } = await api.POST("/api/v1/auth/dev-login", {
      body: email ? { email, display_name: email.split("@")[0] } : {},
    });
    if (!error && response.ok) {
      window.location.href = "/";
      return;
    }
    setError(messageOf(error, "Dev sign-in failed"));
  };

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);

    // Checked here as well as on the server: a typo in a password nobody can
    // see yet is otherwise only discovered at the next sign-in, by which point
    // this account is the sole owner of the instance.
    if (claiming && password !== confirm) {
      setError("The two passwords do not match.");
      return;
    }

    setBusy(true);
    // Two calls rather than one with a computed path: the generated client
    // types each route's body, and casting the path away to share a line
    // would throw that checking out for no gain.
    const { error: apiError, response } = claiming
      ? await api.POST("/api/v1/auth/local/bootstrap", {
          body: { username, password },
        })
      : await api.POST("/api/v1/auth/local/login", {
          body: { username, password },
        });
    setBusy(false);

    if (apiError || !response.ok) {
      setError(
        messageOf(
          apiError,
          claiming
            ? "Could not create the account."
            : "That username and password did not match.",
        ),
      );
      return;
    }
    window.location.reload();
  };

  const nothingAvailable =
    providers && !providers.oidc && !providers.dev_login && !showLocal;

  return (
    <div className="login-screen">
      <div className="login-box">
        <div className="login-title">◆ nook@os</div>
        <div className="muted small">the workspace operating system</div>

        {showLocal && (
          <form className="login-form" onSubmit={submit}>
            {claiming && (
              <p className="muted small login-claim">
                Nobody has signed in yet. The account you create here owns this
                instance.
              </p>
            )}
            <label className="login-field">
              <span className="small muted">Username</span>
              <input
                value={username}
                onChange={(e) => setUsername(e.target.value)}
                autoComplete="username"
                autoFocus
                required
              />
            </label>
            <label className="login-field">
              <span className="small muted">Password</span>
              <input
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                autoComplete={claiming ? "new-password" : "current-password"}
                required
              />
            </label>
            {claiming && (
              <label className="login-field">
                <span className="small muted">Confirm password</span>
                <input
                  type="password"
                  value={confirm}
                  onChange={(e) => setConfirm(e.target.value)}
                  autoComplete="new-password"
                  required
                />
              </label>
            )}
            {error && (
              <div className="small login-error" role="alert">
                {error}
              </div>
            )}
            <button className="btn primary" type="submit" disabled={busy}>
              {busy ? "…" : claiming ? "Create owner account" : "Sign in"}
            </button>
          </form>
        )}

        {showLocal && providers?.oidc && <div className="login-or">or</div>}

        {providers?.oidc && (
          <a className="btn" href="/api/v1/auth/login">
            Sign in with your identity provider
          </a>
        )}
        {providers?.dev_login && !devOpen && (
          <button className="btn" onClick={() => setDevOpen(true)}>
            Sign in with Dev
          </button>
        )}

        {providers?.dev_login && devOpen && (
          <div className="dev-signin">
            <div className="dev-signin-head">
              <span className="faint small">Choose an account — no password</span>
              <button className="btn small" onClick={() => setDevOpen(false)}>
                back
              </button>
            </div>

            {/* Existing accounts first: switching between people you already
                made is the common case, and retyping an address to do it is
                the friction that stops anybody testing roles at all. */}
            {(devAccounts ?? []).map((a) => (
              <button
                key={a.email}
                className="btn dev-account"
                onClick={() => devLogin(a.email)}
                title={`sign in as ${a.email}`}
              >
                <span className="bright">{a.display_name || a.email}</span>
                <span className="faint small mono">{a.email}</span>
                <span className="dev-account-tags">
                  <span className="faint small">{a.tenant_slug}</span>
                  {(a.deployment_roles ?? []).map((r) => (
                    <span key={r} className="dev-role">
                      {r}
                    </span>
                  ))}
                </span>
              </button>
            ))}

            <form
              className="dev-new"
              onSubmit={(e) => {
                e.preventDefault();
                if (devEmail.trim()) void devLogin(devEmail.trim());
              }}
            >
              <input
                value={devEmail}
                onChange={(e) => setDevEmail(e.target.value)}
                placeholder="someone@localhost"
                autoComplete="off"
              />
              <button className="btn" type="submit" disabled={!devEmail.trim()}>
                sign in as new
              </button>
            </form>
          </div>
        )}

        {nothingAvailable && (
          <div className="small" style={{ color: "var(--nook-err)" }}>
            No sign-in method is configured — set OIDC_* on the control plane,
            or leave it unset to use a local account.
          </div>
        )}

        {local?.mode === "oidc" && (
          <p className="muted small login-claim">
            This instance signs in through an identity provider. That choice was
            made on first sign-in and is deliberately one-way.
          </p>
        )}
      </div>
    </div>
  );
}
