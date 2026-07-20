import React from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "@nookos/api";
import {
  applyTokens,
  DEFAULT_THEME,
  Empty,
  Panel,
  Pill,
  type ThemeTokens,
} from "@nookos/ui";
import { KeyRound, Trash2 } from "lucide-react";
import { requireAppPassword, useAppPassword, whenSet } from "../apppassword";
import { enrollPasskey, passkeysSupported } from "../passkey";
import { askConfirm, askText, notify } from "../dialogs";
import {
  desktopPermission,
  playChime,
  requestDesktopPermission,
  useNotify,
} from "../notify";

/** The one password that seals this user's secrets. */
function AppPasswordSettings() {
  const queryClient = useQueryClient();
  const held = useAppPassword((s) => s.passphrase);
  const clear = useAppPassword((s) => s.clear);
  const { data: vault } = useQuery({
    queryKey: ["vault", "status"],
    queryFn: async () => (await api.GET("/api/v1/vault/status", {})).data,
  });

  return (
    <div style={{ padding: 10, display: "grid", gap: 10 }} className="small">
      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
        {vault?.configured ? (
          <>
            <Pill tone="ok">set</Pill>
            <span className="muted">
              {/* The date, not just the fact: it's what tells you the password
                  the app is asking for is one you chose, and roughly when. */}
              {whenSet(vault.created_at)
                ? `Set on ${whenSet(vault.created_at)}. Secrets are sealed with it; it cannot be changed.`
                : "Secrets are sealed with it. It cannot be changed."}
            </span>
          </>
        ) : (
          <>
            <Pill tone="warn">not set</Pill>
            <span className="muted">
              Set it the first time you save a secret, or here.
            </span>
          </>
        )}
      </div>

      <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
        {!vault?.configured && (
          <button
            className="btn primary small"
            onClick={async () => {
              if (await requireAppPassword())
                queryClient.invalidateQueries({ queryKey: ["vault"] });
            }}
          >
            set app password
          </button>
        )}
        {held ? (
          <>
            <Pill tone="ok">unlocked this session</Pill>
            <button className="btn small" onClick={clear}>
              lock
            </button>
          </>
        ) : (
          vault?.configured && <Pill tone="dim">locked</Pill>
        )}
      </div>

      <p className="muted" style={{ marginTop: 2 }}>
        Your app password encrypts secrets before they reach the database, so a
        database dump — even with the server's own key — cannot reveal them.
        NookOS never stores it, which also means nobody can reset it for you.
      </p>

      {vault?.configured && <PasskeySettings />}
    </div>
  );
}

/**
 * Personal access tokens — the credential the `nook` CLI uses to act as you.
 *
 * Worth being clear about why this exists next to node tokens: a node token
 * authenticates a machine and the control plane confines it to that machine,
 * so a script on one box can't start work on another. This one is a person, so
 * it can — which is what `nook login` needs to drive the fleet.
 */
function AccessTokenSettings() {
  const queryClient = useQueryClient();
  const { data: tokens } = useQuery({
    queryKey: ["user-tokens"],
    queryFn: async () => (await api.GET("/api/v1/tokens", {})).data ?? [],
  });

  const mint = async () => {
    const name = await askText({
      title: "New access token",
      description:
        "Names it in this list so you can tell which machine or script to cut off later.",
      label: "What's it for",
      placeholder: "laptop cli",
      confirmLabel: "create",
    });
    if (!name) return;
    const { data, error } = await api.POST("/api/v1/tokens", {
      body: { name, expires_in_days: null },
    });
    if (error || !data) {
      await notify("Could not create the token", JSON.stringify(error));
      return;
    }
    queryClient.invalidateQueries({ queryKey: ["user-tokens"] });
    // Shown once, deliberately: the server keeps only a hash, so this dialog
    // is the single moment the value exists anywhere but in the caller's hands.
    await notify(
      "Copy it now — it won't be shown again",
      "Paste this on the machine that should act as you:",
      { copy: `nook login --token ${data.token}` },
    );
  };

  return (
    <div style={{ padding: 10, display: "grid", gap: 10 }} className="small">
      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
        <button className="btn primary small" onClick={mint}>
          new token
        </button>
        <span className="muted">
          Lets `nook` drive any machine you own — a node token only ever drives
          its own.
        </span>
      </div>
      {(tokens ?? []).length === 0 ? (
        <Empty>No access tokens.</Empty>
      ) : (
        <table className="nook-table">
          <tbody>
            {(tokens ?? []).map((t) => (
              <tr key={t.id}>
                <td className="bright">{t.name || "unnamed"}</td>
                <td className="muted">
                  {t.last_used_at
                    ? `used ${new Date(t.last_used_at).toLocaleDateString()}`
                    : "never used"}
                </td>
                <td>
                  <button
                    className="btn small danger"
                    onClick={async () => {
                      const ok = await askConfirm({
                        title: `Revoke "${t.name || "unnamed"}"`,
                        description:
                          "Anything using it stops working immediately. Machines keep their own node tokens.",
                        confirmLabel: "revoke",
                        danger: true,
                      });
                      if (!ok) return;
                      await api.DELETE("/api/v1/tokens/{id}", {
                        params: { path: { id: t.id } },
                      });
                      queryClient.invalidateQueries({ queryKey: ["user-tokens"] });
                    }}
                  >
                    revoke
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

/**
 * Passkeys that unlock the vault. A passkey holds the app password rather
 * than replacing it, so this is strictly a shortcut: remove them all and
 * typing the password still works.
 */
function PasskeySettings() {
  const queryClient = useQueryClient();
  const [busy, setBusy] = React.useState(false);
  const { data: passkeys } = useQuery({
    queryKey: ["vault", "passkeys"],
    queryFn: async () => (await api.GET("/api/v1/vault/passkeys", {})).data ?? [],
  });
  const { data: me } = useQuery({
    queryKey: ["auth", "me"],
    queryFn: async () => (await api.GET("/api/v1/auth/me")).data,
  });

  const add = async () => {
    if (!passkeysSupported()) {
      await notify(
        "Passkeys aren't available here",
        "They need a secure connection (https) and a device that supports them.",
      );
      return;
    }
    // Wrapping the app password requires holding it.
    const passphrase = await requireAppPassword();
    if (!passphrase) return;

    const label = await askText({
      title: "Name this passkey",
      description: "So you can tell your devices apart later.",
      label: "Name",
      confirmLabel: "continue",
    });
    if (label === null) return;

    setBusy(true);
    try {
      const ok = await enrollPasskey(
        passphrase,
        me?.user.email ?? "nookos user",
        label || "passkey",
      );
      if (ok) queryClient.invalidateQueries({ queryKey: ["vault"] });
    } catch (e) {
      await notify("Couldn't add that passkey", String(e));
    } finally {
      setBusy(false);
    }
  };

  const remove = async (id: string, label: string) => {
    const ok = await askConfirm({
      title: `Remove ${label}?`,
      description:
        "It stops unlocking this vault. Your app password still does — nothing encrypted is lost.",
      confirmLabel: "remove",
      danger: true,
    });
    if (!ok) return;
    await api.DELETE("/api/v1/vault/passkeys/{id}", {
      params: { path: { id } },
    });
    queryClient.invalidateQueries({ queryKey: ["vault"] });
  };

  return (
    <div style={{ borderTop: "1px solid var(--nook-border)", paddingTop: 10 }}>
      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
        <KeyRound size={13} />
        <strong>Passkeys</strong>
        {passkeys?.length ? (
          <Pill tone="ok">unlocks without typing</Pill>
        ) : (
          <Pill tone="dim">none</Pill>
        )}
        <button
          className="btn small"
          onClick={add}
          disabled={busy}
          style={{ marginLeft: "auto" }}
        >
          add passkey
        </button>
      </div>

      {!!passkeys?.length && (
        <table className="nook-table small" style={{ marginTop: 8 }}>
          <tbody>
            {passkeys.map((p) => (
              <tr key={p.id}>
                <td>{p.label}</td>
                <td className="muted">
                  {p.last_used_at
                    ? `used ${new Date(p.last_used_at).toLocaleDateString()}`
                    : "never used"}
                </td>
                <td style={{ textAlign: "right" }}>
                  <button
                    className="btn small icon"
                    title="remove"
                    onClick={() => remove(p.id, p.label)}
                  >
                    <Trash2 size={12} />
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      <p className="muted" style={{ marginTop: 6 }}>
        A passkey stores your app password encrypted with a key only your
        device can produce. NookOS gains nothing it could decrypt — it just
        stops asking you to type.
      </p>
    </div>
  );
}

/** Desktop notification + chime preferences (stored per browser). */
function NotificationSettings() {
  const { desktop, sound, everything, set } = useNotify();
  const [permission, setPermission] = React.useState(desktopPermission());

  const toggleDesktop = async () => {
    if (!desktop) {
      // Browsers require the permission prompt to come from a user gesture.
      const granted = await requestDesktopPermission();
      setPermission(desktopPermission());
      if (!granted) return;
    }
    set({ desktop: !desktop });
  };

  return (
    <div style={{ padding: 10, display: "grid", gap: 10 }} className="small">
      <label className="check-row">
        <input type="checkbox" checked={desktop} onChange={toggleDesktop} />
        <span>Desktop notifications</span>
        {permission === "denied" && (
          <Pill tone="err">blocked in browser settings</Pill>
        )}
        {permission === "unsupported" && <Pill tone="warn">unsupported</Pill>}
      </label>

      <label className="check-row">
        <input
          type="checkbox"
          checked={sound}
          onChange={() => {
            if (!sound) playChime("ok"); // preview when switching on
            set({ sound: !sound });
          }}
        />
        <span>Play a chime</span>
        <button
          type="button"
          className="btn small"
          onClick={(e) => {
            e.preventDefault();
            playChime("ok");
          }}
        >
          test
        </button>
      </label>

      <label className="check-row">
        <input
          type="checkbox"
          checked={everything}
          onChange={() => set({ everything: !everything })}
        />
        <span>Notify for every activity event (noisy)</span>
      </label>

      <p className="muted" style={{ marginTop: 2 }}>
        By default you're notified when work reaches a milestone: clones and
        worktrees finishing, sessions ending, nodes connecting or dropping,
        tasks dispatched, PRs submitted.
      </p>
    </div>
  );
}

export function SettingsPage() {
  const queryClient = useQueryClient();
  const { data: themes } = useQuery({
    queryKey: ["themes"],
    queryFn: async () => (await api.GET("/api/v1/themes")).data ?? [],
  });
  const { data: settings } = useQuery({
    queryKey: ["settings"],
    queryFn: async () => (await api.GET("/api/v1/settings")).data ?? [],
  });

  const activeTheme = String(
    (settings ?? []).find((s) => s.key === "theme")?.value ?? DEFAULT_THEME,
  );

  const pickTheme = async (slug: string, tokens: unknown) => {
    applyTokens(tokens as ThemeTokens);
    await api.PUT("/api/v1/settings/{key}", {
      params: { path: { key: "theme" } },
      body: { value: slug, scope: "user" },
    });
    queryClient.invalidateQueries({ queryKey: ["settings"] });
  };

  return (
    <div className="nook-grid" style={{ gridTemplateColumns: "1fr 1fr" }}>
      <Panel title="Theme">
        {(themes ?? []).length === 0 ? (
          <Empty>No themes installed.</Empty>
        ) : (
          <table className="nook-table">
            <tbody>
              {(themes ?? []).map((t) => (
                <tr key={t.id}>
                  <td className="bright">{t.name}</td>
                  <td className="mono muted">{t.slug}</td>
                  <td>{t.tenant_id === null && <Pill>built-in</Pill>}</td>
                  <td>
                    {activeTheme === t.slug ? (
                      <Pill tone="ok">active</Pill>
                    ) : (
                      <button
                        className="btn small"
                        onClick={() => pickTheme(t.slug, t.tokens)}
                      >
                        use
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Panel>

      <Panel title="App password">
        <AppPasswordSettings />
      </Panel>

      <Panel title="Access tokens">
        <AccessTokenSettings />
      </Panel>

      <Panel title="Notifications">
        <NotificationSettings />
      </Panel>

      <Panel title="Instance">
        <div style={{ padding: 10 }} className="small">
          <p className="muted">
            API docs: <a href="/docs" target="_blank" rel="noreferrer">/docs</a>
          </p>
          <p className="muted" style={{ marginTop: 8 }}>
            MCP endpoint: <span className="mono">/mcp</span> (bearer token from
            your instance config)
          </p>
          <p className="muted" style={{ marginTop: 8 }}>
            Add a machine: Nodes tab → “+ add node”.
          </p>
        </div>
      </Panel>
    </div>
  );
}
