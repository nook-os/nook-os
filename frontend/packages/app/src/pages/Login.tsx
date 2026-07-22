import React from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "@nookos/api";

export function Login() {
  // Only offer sign-in methods this instance actually supports.
  const { data: providers } = useQuery({
    queryKey: ["auth", "providers"],
    queryFn: async () => (await api.GET("/api/v1/auth/providers")).data,
  });

  const devLogin = async () => {
    const { error, response } = await api.POST("/api/v1/auth/dev-login", {
      body: {},
    });
    if (!error && response.ok) window.location.reload();
  };

  return (
    <div className="login-screen">
      <div className="login-box">
        <div className="login-title">◆ nook@os</div>
        <div className="muted small">the workspace operating system</div>
        {providers?.oidc && (
          <a className="btn primary" href="/api/v1/auth/login">
            Sign in with your identity provider
          </a>
        )}
        {providers?.dev_login && (
          <button className="btn" onClick={devLogin}>
            Dev sign-in
          </button>
        )}
        {providers && !providers.oidc && !providers.dev_login && (
          <div className="small" style={{ color: "var(--nook-err)" }}>
            No sign-in method is configured — set OIDC_* (or AUTH_DEV_MODE in
            dev) on the control plane.
          </div>
        )}
      </div>
    </div>
  );
}
