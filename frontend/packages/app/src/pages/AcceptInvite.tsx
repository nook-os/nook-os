// The invite accept landing (MAIN-6). Someone opens the link they were sent,
// which carries `?token=…`. They are already through the auth gate by the time
// they reach here, so we know who they are; accepting consumes the token and
// (on a match) adds them to the inviting tenant, keeping their own.
//
// Every outcome is a plain success page: a good token joins the tenant, and a
// bad/expired/wrong-email one declines with the server's message and leaves
// them in their own tenant — never an error screen, because "this link was for
// a different address" is an answer, not a fault.
import React from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate, useSearchParams } from "react-router-dom";
import { CheckCircle2, Info, MailX } from "lucide-react";
import { api } from "@nookos/api";
import { Empty, Panel } from "@nookos/ui";

export function AcceptInvitePage() {
  const [params] = useSearchParams();
  const token = params.get("token") ?? "";
  const navigate = useNavigate();
  const qc = useQueryClient();

  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });

  // Consume the token exactly once, as soon as we know it. Keyed on the token
  // so re-accepting is a no-op success on the server (idempotent), which means
  // a refresh here is harmless.
  const { data: result, isLoading, isError } = useQuery({
    queryKey: ["accept-invite", token],
    enabled: !!token,
    retry: false,
    queryFn: async () => {
      const { data, error } = await api.POST("/api/v1/invites/accept", {
        body: { token },
      });
      if (error || !data) throw new Error("accept failed");
      return data;
    },
  });

  if (!token) {
    return (
      <CenteredPanel title="Invite">
        <Empty>This link is missing its token.</Empty>
      </CenteredPanel>
    );
  }
  if (isLoading) {
    return (
      <CenteredPanel title="Invite">
        <Empty>Checking your invite…</Empty>
      </CenteredPanel>
    );
  }
  if (isError || !result) {
    return (
      <CenteredPanel title="Invite">
        <Empty>Could not process this invite. Try the link again.</Empty>
      </CenteredPanel>
    );
  }

  // Accepted: switch the active tenant to the one just joined, then drop the
  // person on the board there. Declined: they stay where they are.
  const proceed = async () => {
    if (result.accepted) {
      await api.POST("/api/v1/me/tenant", {
        body: { tenant_id: result.tenant_id },
      });
      await qc.invalidateQueries();
    }
    navigate("/board");
  };

  return (
    <CenteredPanel title={result.accepted ? "You're in" : "Invite"}>
      <div style={{ padding: 16, display: "grid", gap: 12, placeItems: "start" }}>
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          {result.accepted ? (
            <CheckCircle2 size={18} className="ok" />
          ) : (
            <MailX size={18} className="muted" />
          )}
          <strong>{result.message}</strong>
        </div>

        {me && !result.accepted && (
          <p className="muted small" style={{ margin: 0 }}>
            <Info size={11} /> You are signed in as{" "}
            <span className="bright">{me.user.email}</span>. If the invite was
            for a different address, sign in as that address and open the link
            again.
          </p>
        )}

        <button className="btn primary small" onClick={proceed}>
          {result.accepted ? "Go to the board" : "Continue"}
        </button>
      </div>
    </CenteredPanel>
  );
}

function CenteredPanel({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <div style={{ display: "grid", placeItems: "center", padding: 24 }}>
      <div style={{ width: "min(560px, 100%)" }}>
        <Panel title={title}>{children}</Panel>
      </div>
    </div>
  );
}
