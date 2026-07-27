// The invite accept landing (MAIN-6, extended by MAIN-97).
//
// Someone opens the link they were sent, which carries `?token=…`. Two worlds
// meet here:
//
//  • SIGNED IN — we already know who they are, so accepting consumes the token
//    and (on a match) adds them to the inviting tenant, keeping their own. Every
//    outcome is a plain success page: a good token joins, a bad/expired/wrong-
//    email one declines with the server's message. The one decline we surface
//    specially is an UNVERIFIED email (AC-5): that is fixable by checking the
//    inbox, not a dead end.
//
//  • SIGNED OUT — the generic login screen would lose the token, stranding the
//    invitee. Instead we preview the invite (unauthenticated) and, for a valid
//    token, show "«Inviter» invited you to «tenant»" with sign-in / create-
//    account actions that carry `next=/accept?token=…` back through auth
//    (MAIN-97). An invalid token shows the same generic state the preview does.
import React from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useSearchParams } from "react-router-dom";
import { CheckCircle2, Info, MailCheck, MailX, MailWarning } from "lucide-react";
import { api } from "@nookos/api";
import { Empty, Panel } from "@nookos/ui";

export function AcceptInvitePage() {
  const [params] = useSearchParams();
  const token = params.get("token") ?? "";
  const navigate = useNavigate();
  const qc = useQueryClient();

  const { data: me, isLoading: meLoading } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
  const signedIn = !!me;

  // Signed-in path: consume the token exactly once, keyed on the token so a
  // refresh is a harmless idempotent no-op on the server.
  const {
    data: result,
    isLoading: accepting,
    isError,
  } = useQuery({
    queryKey: ["accept-invite", token],
    enabled: !!token && signedIn,
    retry: false,
    queryFn: async () => {
      const { data, error } = await api.POST("/api/v1/invites/accept", {
        body: { token },
      });
      if (error || !data) throw new Error("accept failed");
      return data;
    },
  });

  // Signed-out path: preview the invite (unauthenticated) so the landing can
  // name who invited whom before sign-in.
  const { data: preview, isLoading: previewLoading } = useQuery({
    queryKey: ["invite-preview", token],
    enabled: !!token && !signedIn && !meLoading,
    retry: false,
    queryFn: async () =>
      (
        await api.GET("/api/v1/invites/preview", {
          params: { query: { token } },
        })
      ).data ?? null,
  });

  // Which sign-in methods this instance offers, so the landing never shows a
  // dead button.
  const { data: providers } = useQuery({
    queryKey: ["auth", "providers"],
    queryFn: async () => (await api.GET("/api/v1/auth/providers")).data,
    enabled: !signedIn,
  });

  if (!token) {
    return (
      <CenteredPanel title="Invite">
        <Empty>This link is missing its token.</Empty>
      </CenteredPanel>
    );
  }

  // ── Signed out: the invite landing ────────────────────────────────────────
  if (!signedIn) {
    if (meLoading || previewLoading) {
      return (
        <CenteredPanel title="Invite">
          <Empty>Checking your invite…</Empty>
        </CenteredPanel>
      );
    }
    if (!preview || !preview.valid) {
      return (
        <CenteredPanel title="Invite">
          <Empty>
            This invitation is no longer valid. Ask whoever invited you to send a
            fresh link.
          </Empty>
        </CenteredPanel>
      );
    }
    return (
      <InviteLanding
        token={token}
        tenant={preview.tenant}
        inviter={preview.inviter}
        email={preview.email}
        oidc={providers?.oidc === true}
        local={providers?.local === true}
        dev={providers?.dev_login === true}
      />
    );
  }

  // ── Signed in: the auto-accept flow ───────────────────────────────────────
  if (accepting) {
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

  // A decline for an UNVERIFIED email is not a dead end — the IdP has (or will)
  // send a verification email, and the same link works once it is confirmed.
  const unverified =
    !result.accepted && /verify your email/i.test(result.message);

  return (
    <CenteredPanel title={result.accepted ? "You're in" : "Invite"}>
      <div style={{ padding: 16, display: "grid", gap: 12, placeItems: "start" }}>
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          {result.accepted ? (
            <CheckCircle2 size={18} className="ok" />
          ) : unverified ? (
            <MailWarning size={18} className="muted" />
          ) : (
            <MailX size={18} className="muted" />
          )}
          <strong>
            {unverified
              ? "Verify your email to accept this invite"
              : result.message}
          </strong>
        </div>

        {unverified && (
          <p className="muted small" style={{ margin: 0 }}>
            <Info size={11} /> Check your inbox for a verification email from your
            identity provider, confirm your address
            {me?.user.email ? (
              <>
                {" "}
                (<span className="bright">{me.user.email}</span>)
              </>
            ) : null}
            , then open this invite link again.
          </p>
        )}

        {me && !result.accepted && !unverified && (
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

/** The signed-out landing: who invited you, and how to get back here after
 *  authenticating. Every action carries `next=/accept?token=…` so the token
 *  survives the round trip through the IdP or the login screen (AC-3). */
function InviteLanding({
  token,
  tenant,
  inviter,
  email,
  oidc,
  local,
  dev,
}: {
  token: string;
  tenant: string;
  inviter: string;
  email: string;
  oidc: boolean;
  local: boolean;
  dev: boolean;
}) {
  const acceptPath = `/accept?token=${encodeURIComponent(token)}`;
  const enc = encodeURIComponent(acceptPath);
  // OIDC sign-in / create-account go straight to the backend authorize flow,
  // which round-trips through the IdP and returns to `next`.
  const oidcSignIn = `/api/v1/auth/login?next=${enc}`;
  // Create account: ask the IdP to open its sign-up screen. We do NOT pass a
  // login_hint — the preview only holds a MASKED email (privacy), so there is
  // no full address to hint with; `prompt=create` alone is the correct minimum,
  // and IdPs that ignore it still work.
  const oidcCreate = `${oidcSignIn}&prompt=create`;
  // Local / dev sign-in reuses the full login screen, which itself decides what
  // is usable; `next` carries the invitee back here afterwards.
  const passwordSignIn = `/login?next=${enc}`;

  // Whether this instance can create local accounts *right now* (MAIN-98 AC-4).
  // `/auth/providers` says local is configured; `/auth/local/status` says it is
  // actually available to sign up against — the form is gated on the latter.
  const { data: localStatus } = useQuery({
    queryKey: ["auth", "local-status"],
    queryFn: async () => (await api.GET("/api/v1/auth/local/status")).data ?? null,
  });
  const canCreateLocal = localStatus?.available === true;

  return (
    <CenteredPanel title="You're invited">
      <div style={{ padding: 16, display: "grid", gap: 16, placeItems: "start" }}>
        <div>
          <p style={{ margin: 0, fontSize: 15 }}>
            <strong className="bright">{inviter}</strong> invited you to{" "}
            <strong className="bright">{tenant}</strong>.
          </p>
          <p className="muted small" style={{ margin: "6px 0 0" }}>
            Invited as <span className="mono">{email}</span>. Sign in with that
            address to accept.
          </p>
        </div>

        <div style={{ display: "grid", gap: 8, width: "100%", maxWidth: 320 }}>
          {oidc && (
            <>
              <a className="btn primary" href={oidcSignIn}>
                Sign in to accept
              </a>
              <a className="btn" href={oidcCreate}>
                Create an account
              </a>
            </>
          )}
          {!oidc && (local || dev) && (
            <Link className="btn primary" to={passwordSignIn}>
              Sign in to accept
            </Link>
          )}
          {oidc && (local || dev) && (
            <Link className="btn small" to={passwordSignIn}>
              Other sign-in options
            </Link>
          )}
          {!oidc && !local && !dev && (
            <Empty>
              No sign-in method is configured on this instance yet.
            </Empty>
          )}
        </div>

        {/* Local instances: let the invitee make an account without leaving the
            invite. OIDC instances hand account creation to the IdP above, so the
            form only shows when local sign-up is actually available. */}
        {canCreateLocal && (
          <CreateAccountForm
            token={token}
            email={email}
            signInPath={passwordSignIn}
          />
        )}
      </div>
    </CenteredPanel>
  );
}

/**
 * Create-account form on the invite landing (MAIN-98 AC-4). No email field —
 * the address comes from the invite server-side; the masked invite email is
 * shown read-only for context. On success the account exists but is unverified,
 * so we point the invitee at their inbox and keep the sign-in action (with the
 * token preserved) so they land back here after verifying.
 */
function CreateAccountForm({
  token,
  email,
  signInPath,
}: {
  token: string;
  email: string;
  signInPath: string;
}) {
  const [name, setName] = React.useState("");
  const [username, setUsername] = React.useState("");
  const [password, setPassword] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const [done, setDone] = React.useState(false);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (busy) return;
    setError(null);
    setBusy(true);
    const { error } = await api.POST("/api/v1/invites/register", {
      body: { token, name, username, password },
    });
    setBusy(false);
    if (error) {
      // A bad or duplicate username comes back as `{ error }` — surface it
      // inline rather than crashing.
      setError(
        (error as { error?: string })?.error ||
          "Could not create your account. Try again.",
      );
      return;
    }
    setDone(true);
  };

  if (done) {
    return (
      <div
        style={{
          borderTop: "1px solid var(--nook-border)",
          paddingTop: 12,
          display: "grid",
          gap: 10,
          width: "100%",
          maxWidth: 320,
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <MailCheck size={16} className="ok" />
          <strong>Account created</strong>
        </div>
        <p className="muted small" style={{ margin: 0 }}>
          <Info size={11} /> Check your inbox for a verification email, confirm
          your address, then sign in here to accept the invite.
        </p>
        <Link className="btn primary" to={signInPath}>
          Sign in to accept
        </Link>
      </div>
    );
  }

  return (
    <form
      onSubmit={submit}
      style={{
        borderTop: "1px solid var(--nook-border)",
        paddingTop: 12,
        display: "grid",
        gap: 10,
        width: "100%",
        maxWidth: 320,
      }}
    >
      <div>
        <strong>Create an account</strong>
        <p className="muted small" style={{ margin: "4px 0 0" }}>
          Sign up as <span className="mono">{email}</span> to accept.
        </p>
      </div>

      <div className="field">
        <label>Name</label>
        <input
          className="input"
          type="text"
          autoComplete="name"
          placeholder="Ada Lovelace"
          value={name}
          onChange={(e) => setName(e.target.value)}
        />
      </div>

      <div className="field">
        <label>Username</label>
        <input
          className="input"
          type="text"
          autoComplete="username"
          autoCapitalize="off"
          autoCorrect="off"
          spellCheck={false}
          placeholder="ada"
          value={username}
          onChange={(e) => setUsername(e.target.value)}
        />
      </div>

      <div className="field">
        <label>Password</label>
        <input
          className="input"
          type="password"
          autoComplete="new-password"
          autoCapitalize="off"
          autoCorrect="off"
          spellCheck={false}
          value={password}
          onChange={(e) => setPassword(e.target.value)}
        />
      </div>

      {error && (
        <p className="err small" style={{ margin: 0 }} role="alert">
          {error}
        </p>
      )}

      <button
        className="btn primary"
        type="submit"
        disabled={busy || !name.trim() || !username.trim() || !password}
      >
        {busy ? "Creating…" : "Create account"}
      </button>
    </form>
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
