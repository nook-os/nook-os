// Invite people into your tenant (MAIN-6, emailed in MAIN-7).
//
// An owner/admin creates a pending invite for an email + role; the server
// emails the invitee the accept link (when mail is configured) AND returns it
// here with a copy button, so the copy-link path always works. The invitee opens
// the link, signs in as that email, and is added to the tenant. A resend
// re-emails a fresh link. Members can't manage invites, so the whole panel is
// hidden for them rather than rendering buttons that 403.
import React, { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Mail, Plus, Send, Trash2 } from "lucide-react";
import { api, type Invite } from "@nookos/api";
import { Empty, Panel, Pill, Select } from "@nookos/ui";
import { askConfirm, notify } from "./dialogs";

export function Invites() {
  const qc = useQueryClient();
  const { data: me } = useQuery({
    queryKey: ["me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data ?? null,
  });
  const tenantId = me?.tenant.id ?? null;
  const canManage = me ? ["owner", "admin"].includes(me.user.role) : false;

  const { data: invites } = useQuery({
    queryKey: ["invites", tenantId],
    enabled: !!tenantId && canManage,
    queryFn: async () =>
      (
        await api.GET("/api/v1/tenants/{id}/invites", {
          params: { path: { id: tenantId! } },
        })
      ).data ?? [],
  });

  const [adding, setAdding] = useState(false);
  const [email, setEmail] = useState("");
  const [role, setRole] = useState("member");
  const [busy, setBusy] = useState(false);

  // Members never see this: they cannot list or create, and the panel would be
  // an empty box of disabled buttons.
  if (!canManage) return null;

  const bust = () => qc.invalidateQueries({ queryKey: ["invites"] });

  const create = async () => {
    if (!tenantId) return;
    setBusy(true);
    const { data, error } = await api.POST("/api/v1/tenants/{id}/invites", {
      params: { path: { id: tenantId } },
      body: { email: email.trim(), role },
    });
    setBusy(false);
    if (error || !data) {
      await notify("Could not create the invite", messageOf(error));
      return;
    }
    setAdding(false);
    setEmail("");
    setRole("member");
    bust();
    // The link is the deliverable. Shown here with a copy button because there
    // is no email step yet (MAIN-7); re-inviting the same address mints a fresh
    // one if this is lost.
    if (data.accept_url) {
      await notify(
        `Invite sent to ${data.email}`,
        "We emailed them this link (if mail is configured). You can also copy it — they sign in as this email to join your tenant:",
        { copy: data.accept_url },
      );
    }
  };

  const resend = async (inv: Invite) => {
    if (!tenantId) return;
    const { data, error } = await api.POST(
      "/api/v1/tenants/{id}/invites/{invite}/resend",
      { params: { path: { id: tenantId, invite: inv.id } } },
    );
    if (error || !data) {
      await notify("Could not resend the invite", messageOf(error));
      return;
    }
    bust();
    // Resend rotates the token, so the OLD link stops working — surface the
    // fresh one for the copy-link path alongside the email.
    await notify(
      `Invite re-sent to ${data.email}`,
      "A fresh link was emailed (if mail is configured). The previous link no longer works; copy the new one if you need it:",
      data.accept_url ? { copy: data.accept_url } : undefined,
    );
  };

  const revoke = async (inv: Invite) => {
    if (!tenantId) return;
    const ok = await askConfirm({
      title: `Revoke the invite for ${inv.email}?`,
      description: "Their link stops working. You can invite them again later.",
      confirmLabel: "revoke",
      danger: true,
    });
    if (!ok) return;
    await api.DELETE("/api/v1/tenants/{id}/invites/{invite}", {
      params: { path: { id: tenantId, invite: inv.id } },
    });
    bust();
  };

  return (
    <Panel
      title="Invites"
      actions={
        !adding && (
          <button className="btn small" onClick={() => setAdding(true)}>
            <Plus size={12} /> invite
          </button>
        )
      }
    >
      <div style={{ padding: 10, display: "grid", gap: 10 }} className="small">
        <p className="muted" style={{ margin: 0 }}>
          Bring someone into <span className="bright">{me?.tenant.name}</span>.
          They join by opening the link and signing in as the invited email —
          keeping their own tenant, and gaining this one.
        </p>

        {adding && (
          <div className="chan-form">
            <div className="chan-row">
              <span className="faint small">Email</span>
              <input
                className="chan-input"
                type="email"
                value={email}
                placeholder="person@example.com"
                autoComplete="off"
                onChange={(e) => setEmail(e.target.value)}
              />
            </div>
            <div className="chan-row">
              <span className="faint small">Role</span>
              <Select
                value={role}
                onChange={setRole}
                options={[
                  { value: "member", label: "member" },
                  { value: "admin", label: "admin" },
                ]}
              />
            </div>
            <div className="chan-actions">
              <button className="btn small" onClick={() => setAdding(false)}>
                cancel
              </button>
              <button
                className="btn small primary"
                onClick={create}
                disabled={busy || !email.includes("@")}
              >
                {busy ? "creating…" : "create invite"}
              </button>
            </div>
          </div>
        )}

        {(invites ?? []).length === 0 ? (
          <Empty>
            <Mail size={13} /> No pending invites.
          </Empty>
        ) : (
          <table className="nook-table">
            <tbody>
              {(invites ?? []).map((inv) => (
                <tr key={inv.id}>
                  <td className="bright">{inv.email}</td>
                  <td>
                    <Pill tone={inv.role === "admin" ? "warn" : "dim"}>
                      {inv.role}
                    </Pill>
                  </td>
                  <td className="muted">
                    expires {new Date(inv.expires_at).toLocaleDateString()}
                  </td>
                  <td style={{ textAlign: "right", whiteSpace: "nowrap" }}>
                    <button
                      className="btn small icon"
                      title="resend the invite email"
                      onClick={() => resend(inv)}
                    >
                      <Send size={12} />
                    </button>
                    <button
                      className="btn small danger icon"
                      title="revoke"
                      onClick={() => revoke(inv)}
                    >
                      <Trash2 size={12} />
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </Panel>
  );
}

function messageOf(error: unknown): string {
  if (typeof error === "object" && error && "error" in error) {
    return String((error as { error: unknown }).error);
  }
  return JSON.stringify(error);
}
