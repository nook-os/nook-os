import React, { useState } from "react";
import { api } from "@nookos/api";

export function Login() {
  const [devError, setDevError] = useState<string | null>(null);

  const devLogin = async () => {
    const { error, response } = await api.POST("/api/v1/auth/dev-login", {
      body: {},
    });
    if (error || !response.ok) {
      setDevError("dev login is disabled on this instance");
      return;
    }
    window.location.reload();
  };

  return (
    <div className="login-screen">
      <div className="login-box">
        <div className="login-title">◆ NOOKOS</div>
        <div className="muted small">the workspace operating system</div>
        <a className="btn primary" href="/api/v1/auth/login">
          Sign in with your identity provider
        </a>
        <button className="btn" onClick={devLogin}>
          Dev sign-in
        </button>
        {devError && <div className="small" style={{ color: "var(--nook-err)" }}>{devError}</div>}
      </div>
    </div>
  );
}
