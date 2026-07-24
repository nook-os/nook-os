// The email-verification landing (MAIN-30). Opening the link someone was sent
// consumes its `?token=…` — token-authenticated, so it works in any browser,
// signed in or not. Every outcome is a plain page: a good token verifies the
// address, a used/expired one declines with the server's message.
import React from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate, useSearchParams } from "react-router-dom";
import { CheckCircle2, MailX } from "lucide-react";
import { api } from "@nookos/api";
import { Empty, Panel } from "@nookos/ui";

export function VerifyEmailPage() {
  const [params] = useSearchParams();
  const token = params.get("token") ?? "";
  const navigate = useNavigate();
  const qc = useQueryClient();

  // Consume once, keyed on the token; the server makes a repeat a no-op success.
  const { data: result, isLoading, isError } = useQuery({
    queryKey: ["verify-email", "confirm", token],
    enabled: !!token,
    retry: false,
    queryFn: async () => {
      const { data, error } = await api.POST("/api/v1/auth/verify-email/confirm", {
        body: { token },
      });
      if (error || !data) throw new Error("confirm failed");
      // A signed-in Settings view should reflect the new state.
      qc.invalidateQueries({ queryKey: ["verify-email"] });
      return data;
    },
  });

  const body = () => {
    if (!token) return <Empty>This link is missing its token.</Empty>;
    if (isLoading) return <Empty>Confirming your email…</Empty>;
    if (isError || !result)
      return <Empty>Could not confirm this link. Try requesting a new one.</Empty>;
    return (
      <div style={{ padding: 16, display: "grid", gap: 12, placeItems: "start" }}>
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          {result.verified ? (
            <CheckCircle2 size={18} className="ok" />
          ) : (
            <MailX size={18} className="muted" />
          )}
          <strong>{result.message}</strong>
        </div>
        <button className="btn primary small" onClick={() => navigate("/settings")}>
          Go to settings
        </button>
      </div>
    );
  };

  return (
    <div style={{ display: "grid", placeItems: "center", padding: 24 }}>
      <div style={{ width: "min(560px, 100%)" }}>
        <Panel title={result?.verified ? "Email verified" : "Verify email"}>
          {body()}
        </Panel>
      </div>
    </div>
  );
}
