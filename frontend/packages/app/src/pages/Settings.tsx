import React from "react";
import { OrgVisibility } from "../OrgVisibility";
import { SectionedPage, type PageSection } from "../SectionedPage";
import { NotificationChannels } from "../NotificationChannels";
import { GitCredentials } from "../GitCredentials";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  api,
  type UserToken,
  type VaultPasskey,
} from "@nookos/api";
import {
  applyTokens,
  DataList,
  DEFAULT_THEME,
  Empty,
  Markdown,
  Panel,
  Pill,
  RowAction,
  RowActions,
  SearchInput,
  type DataColumn,
  type ThemeTokens,
} from "@nookos/ui";
import { Eye, KeyRound, Palette, Pencil, Plus, Trash2 } from "lucide-react";
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
  const [search, setSearch] = React.useState("");
  const { data: tokens } = useQuery({
    queryKey: ["user-tokens"],
    queryFn: async () => (await api.GET("/api/v1/tokens", {})).data ?? [],
  });

  const revoke = async (t: UserToken) => {
    const ok = await askConfirm({
      title: `Revoke "${t.name || "unnamed"}"`,
      description:
        "Anything using it stops working immediately. Machines keep their own node tokens.",
      confirmLabel: "revoke",
      danger: true,
    });
    if (!ok) return;
    await api.DELETE("/api/v1/tokens/{id}", { params: { path: { id: t.id } } });
    queryClient.invalidateQueries({ queryKey: ["user-tokens"] });
  };

  // Client-side filter: a per-user list is small enough that no backend paging
  // is needed (AC-3), but the search stays consistent with the other tables.
  const term = search.trim().toLowerCase();
  const rows = (tokens ?? []).filter(
    (t) => !term || (t.name ?? "").toLowerCase().includes(term),
  );
  const columns: DataColumn<UserToken>[] = [
    { key: "name", header: "Name", className: "bright", cell: (t) => t.name || "unnamed" },
    {
      key: "used",
      header: "Last used",
      className: "muted",
      cell: (t) =>
        t.last_used_at ? `used ${new Date(t.last_used_at).toLocaleDateString()}` : "never used",
    },
    {
      key: "actions",
      header: "",
      cell: (t) => (
        <RowActions>
          <RowAction icon={Trash2} danger title="revoke this token" onClick={() => revoke(t)} />
        </RowActions>
      ),
    },
  ];

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
      {(tokens ?? []).length > 0 && (
        <SearchInput
          onSearch={setSearch}
          placeholder="Search tokens…"
          ariaLabel="Search access tokens"
        />
      )}
      <DataList
        columns={columns}
        rows={rows}
        rowKey={(t) => t.id}
        filtered={search.trim().length > 0}
        empty="No access tokens."
        noResults="No matches."
      />
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
  const [search, setSearch] = React.useState("");
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

  const term = search.trim().toLowerCase();
  const filteredPasskeys = (passkeys ?? []).filter(
    (p) => !term || p.label.toLowerCase().includes(term),
  );
  const passkeyColumns: DataColumn<VaultPasskey>[] = [
    { key: "label", header: "Passkey", cell: (p) => p.label },
    {
      key: "used",
      header: "Last used",
      className: "muted",
      cell: (p) =>
        p.last_used_at ? `used ${new Date(p.last_used_at).toLocaleDateString()}` : "never used",
    },
    {
      key: "actions",
      header: "",
      cell: (p) => (
        <RowActions>
          <RowAction
            icon={Trash2}
            danger
            title="remove this passkey"
            onClick={() => remove(p.id, p.label)}
          />
        </RowActions>
      ),
    },
  ];

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
        <div style={{ marginTop: 8 }}>
          {passkeys.length > 1 && (
            <SearchInput
              onSearch={setSearch}
              placeholder="Search passkeys…"
              ariaLabel="Search passkeys"
            />
          )}
          <DataList
            columns={passkeyColumns}
            rows={filteredPasskeys}
            rowKey={(p) => p.id}
            filtered={search.trim().length > 0}
            empty="No passkeys."
            noResults="No matches."
          />
        </div>
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

/**
 * Email verification for local accounts (MAIN-30). OIDC users are verified by
 * their IdP and see that state, with no action to take; a local user can request
 * a verification link and see when it lands.
 */
function EmailVerification() {
  const queryClient = useQueryClient();
  const [busy, setBusy] = React.useState(false);
  const { data: status } = useQuery({
    queryKey: ["verify-email", "status"],
    queryFn: async () =>
      (await api.GET("/api/v1/auth/verify-email/status")).data ?? null,
  });

  const request = async () => {
    setBusy(true);
    const { data, error } = await api.POST("/api/v1/auth/verify-email/request", {});
    setBusy(false);
    if (error || !data) {
      await notify("Could not request verification", JSON.stringify(error));
      return;
    }
    // A best-effort send: `sent` tells apart "check your inbox" from a mail
    // misconfiguration, which is a state to show rather than an error to hide.
    await notify(data.sent ? "Verification email sent" : "Not sent", data.message);
    queryClient.invalidateQueries({ queryKey: ["verify-email"] });
  };

  return (
    <div style={{ padding: 10, display: "grid", gap: 10 }} className="small">
      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
        <span className="muted">{status?.email}</span>
        {status?.verified ? (
          <Pill tone="ok">verified</Pill>
        ) : (
          <Pill tone="warn">unverified</Pill>
        )}
      </div>
      {status && !status.verified && status.can_request && (
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <button className="btn primary small" onClick={request} disabled={busy}>
            {busy ? "sending…" : "verify email"}
          </button>
          <span className="muted">
            We’ll email a link. Open it to confirm this address.
          </span>
        </div>
      )}
      {status && !status.verified && !status.can_request && (
        <p className="muted" style={{ margin: 0 }}>
          Your email is verified by your identity provider, not here.
        </p>
      )}
    </div>
  );
}

/** The message a fan-out produced, phrased honestly: who got it now, and who
 *  was offline (offline is normal — those converge on reconnect). */
function fanoutText(r: { delivered_to?: string[]; offline?: string[] } | null): string {
  const delivered = r?.delivered_to ?? [];
  const offline = r?.offline ?? [];
  const lines: string[] = [];
  lines.push(
    delivered.length
      ? `Delivered to: ${delivered.join(", ")}`
      : "No connected machines received it yet.",
  );
  if (offline.length) {
    lines.push(
      `Offline (they converge on reconnect): ${offline.join(", ")}`,
    );
  }
  return lines.join("\n");
}

/** The server's error message, or a stringified fallback. NookOS ApiError
 *  serialises as `{ error }`. */
function errText(error: unknown): string {
  if (error && typeof error === "object") {
    const e = error as { error?: unknown; message?: unknown };
    if (typeof e.error === "string") return e.error;
    if (typeof e.message === "string") return e.message;
  }
  return JSON.stringify(error);
}

/** Taught skills — the one user-mutable skill surface (the managed set is
 *  immutable and invisible here). List, view, edit, create, forget, over the
 *  existing teach/unteach endpoints (MAIN-106). */
function TaughtSkillsSettings() {
  const queryClient = useQueryClient();
  const { data: skills } = useQuery({
    queryKey: ["skills"],
    queryFn: async () => (await api.GET("/api/v1/skills")).data ?? [],
  });
  // Which skill's rendered body is expanded inline (the markdown-stack view).
  const [open, setOpen] = React.useState<string | null>(null);
  const { data: full } = useQuery({
    queryKey: ["skill", open],
    enabled: !!open,
    queryFn: async () =>
      (
        await api.GET("/api/v1/skills/{name}", {
          params: { path: { name: open as string } },
        })
      ).data ?? null,
  });

  const refresh = () => {
    queryClient.invalidateQueries({ queryKey: ["skills"] });
    if (open) queryClient.invalidateQueries({ queryKey: ["skill", open] });
  };

  // Create and update are the same teach upsert (AC-5). Returns whether it stuck.
  const teach = async (name: string | null, content: string) => {
    const { data, error } = await api.POST("/api/v1/skills", {
      body: { name: name ?? undefined, content },
    });
    if (error || !data) {
      await notify("Couldn't save that skill", errText(error));
      return false;
    }
    refresh();
    await notify(`Saved ${data.skill.name}`, fanoutText(data));
    return true;
  };

  const create = async () => {
    const name = await askText({
      title: "New skill",
      description: "A name (letters, numbers, dot, dash, underscore), or leave blank to read it from the file's frontmatter.",
      label: "name",
      placeholder: "code-review",
      confirmLabel: "next",
    });
    if (name === null) return;
    const content = await askText({
      title: name ? `Content for ${name}` : "Skill content",
      label: "SKILL.md",
      multiline: true,
      placeholder: "---\nname: code-review\ndescription: …\n---\n\n# …",
      confirmLabel: "teach the fleet",
    });
    if (content === null || !content.trim()) return;
    await teach(name.trim() || null, content);
  };

  const edit = async (name: string) => {
    const cur = (
      await api.GET("/api/v1/skills/{name}", { params: { path: { name } } })
    ).data;
    if (!cur) {
      await notify("Couldn't open that skill", "It may have just been removed.");
      return;
    }
    const content = await askText({
      title: `Edit ${name}`,
      label: "SKILL.md",
      multiline: true,
      value: cur.content,
      confirmLabel: "save & re-teach",
    });
    if (content === null || content === cur.content) return;
    await teach(name, content);
  };

  const forget = async (name: string) => {
    const ok = await askConfirm({
      title: `Forget ${name}?`,
      description:
        "Removes it from the store and every connected machine; offline machines converge by omission.",
      confirmLabel: "forget",
      danger: true,
    });
    if (!ok) return;
    const { data, error } = await api.DELETE("/api/v1/skills/{name}", {
      params: { path: { name } },
    });
    if (error) {
      await notify("Couldn't forget that skill", errText(error));
      return;
    }
    if (open === name) setOpen(null);
    refresh();
    await notify(
      `Forgot ${name}`,
      fanoutText(data as { delivered_to?: string[]; offline?: string[] } | null),
    );
  };

  return (
    <Panel
      title="Taught skills"
      actions={
        <button className="btn primary small" onClick={create}>
          <Plus size={12} /> new skill
        </button>
      }
    >
      {(skills ?? []).length === 0 ? (
        <Empty>
          Nothing taught yet. Teach one here, or from a machine with{" "}
          <span className="mono">nook teach &lt;path&gt;</span>.
        </Empty>
      ) : (
        <table className="nook-table">
          <thead>
            <tr>
              <th>Skill</th>
              <th>Size</th>
              <th>Updated</th>
              <th>By</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {(skills ?? []).map((s) => (
              <React.Fragment key={s.id}>
                <tr>
                  <td className="bright mono">{s.name}</td>
                  <td className="muted">{Math.max(1, Math.round(s.size / 1024))} KiB</td>
                  <td className="muted">
                    {new Date(s.updated_at).toLocaleDateString()}
                  </td>
                  <td className="muted">{s.updated_by ?? "—"}</td>
                  <td style={{ textAlign: "right", whiteSpace: "nowrap" }}>
                    <span style={{ display: "inline-flex", gap: 6, justifyContent: "flex-end" }}>
                      <button
                        className="btn small"
                        title={open === s.name ? "hide" : "view"}
                        onClick={() => setOpen(open === s.name ? null : s.name)}
                      >
                        <Eye size={12} /> {open === s.name ? "hide" : "view"}
                      </button>
                      <button className="btn small" title="edit" onClick={() => edit(s.name)}>
                        <Pencil size={12} /> edit
                      </button>
                      <button className="btn small" title="forget" onClick={() => forget(s.name)}>
                        <Trash2 size={12} /> forget
                      </button>
                    </span>
                  </td>
                </tr>
                {open === s.name && (
                  <tr>
                    <td colSpan={5}>
                      {full ? (
                        <div style={{ padding: 8, maxHeight: 360, overflow: "auto" }}>
                          <Markdown src={full.content} />
                        </div>
                      ) : (
                        <div className="muted" style={{ padding: 8 }}>
                          loading…
                        </div>
                      )}
                    </td>
                  </tr>
                )}
              </React.Fragment>
            ))}
          </tbody>
        </table>
      )}
    </Panel>
  );
}

/** Theme picking, in its own section component so the queries only run when
 *  somebody is actually looking at appearance. */
function AppearanceSettings() {
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
                    <RowActions>
                      <Pill tone="ok">active</Pill>
                    </RowActions>
                  ) : (
                    <RowActions>
                      <RowAction
                        icon={Palette}
                        label="use"
                        title={`switch to ${t.name}`}
                        onClick={() => pickTheme(t.slug, t.tokens)}
                      />
                    </RowActions>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Panel>
  );
}

/** One tenant-wide on/off switch, rendered the way the loops switch always
 *  was. Extracted because there are two of them now and their copy is the only
 *  difference — the shape drifting apart would make one look less official
 *  than the other. */
function TenantSwitch({
  label,
  settingKey,
  on,
  onCopy,
  offCopy,
  cli,
  flip,
  testid,
}: {
  label: string;
  settingKey: string;
  on: boolean;
  onCopy: string;
  offCopy: string;
  cli: string;
  flip: (key: string, next: boolean) => Promise<void>;
  testid?: string;
}) {
  return (
    <>
      <div className="loops-switch" data-testid={testid}>
        <div>
          <div className="bright">
            {label} {on ? <Pill tone="ok">on</Pill> : <Pill>off</Pill>}
          </div>
          <div className="faint small">{on ? onCopy : offCopy}</div>
        </div>
        <button
          className={`btn small${on ? "" : " primary"}`}
          onClick={() => void flip(settingKey, !on)}
          aria-pressed={on}
        >
          {on ? "Turn off" : "Turn on"}
        </button>
      </div>
      <div className="faint small">
        Also: <span className="mono">{cli}</span>. Takes effect within a poll
        interval; no restart.
      </div>
    </>
  );
}

/** The tenant's automation switches (MAIN-239 + the reconcile twin). Both are
 *  team-wide, which is why this whole section carries the `team` badge in the
 *  nav — the blast radius is everyone, not this browser. */
function AutomationSettings() {
  const queryClient = useQueryClient();
  const { data: settings } = useQuery({
    queryKey: ["settings"],
    queryFn: async () => (await api.GET("/api/v1/settings")).data ?? [],
  });
  const isOn = (key: string) =>
    (settings ?? []).find((s) => s.key === key && s.scope === "tenant")?.value === true;

  const flip = async (key: string, next: boolean) => {
    await api.PUT("/api/v1/settings/{key}", {
      params: { path: { key } },
      body: { value: next, scope: "tenant" },
    });
    queryClient.invalidateQueries({ queryKey: ["settings"] });
  };

  return (
    <Panel title="Automation">
      <div style={{ padding: 10, display: "grid", gap: 14 }} className="small">
        <TenantSwitch
          label="Loop machinery is"
          settingKey="loops.enabled"
          on={isOn("loops.enabled")}
          onCopy="Queued spec and decompose jobs are dispatched to eligible executors."
          offCopy="Off by default. Jobs you start are queued and wait here — nothing is lost, and nothing runs until you turn this on."
          cli="nook operator loops on|off|status"
          flip={flip}
          testid="loops-switch"
        />
        <TenantSwitch
          label="Session reconciling is"
          settingKey="sessions.reconcile.enabled"
          on={isOn("sessions.reconcile.enabled")}
          onCopy="Workspaces with a session spec converge onto their nodes — clones made, sessions started and kept running."
          offCopy="Off by default. A workspace can declare sessions and show 'reconciling off' forever until this is turned on."
          cli="nook operator reconcile on|off|status"
          flip={flip}
          testid="reconcile-switch"
        />
        <p className="muted" style={{ margin: 0 }}>
          Both apply to this whole team, not just you.
        </p>
      </div>
    </Panel>
  );
}

function InstanceInfo() {
  return (
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
  );
}

export function SettingsPage() {
  // The registry IS the information architecture. Grouped by who a section
  // affects — You / Team / Instance — because "is this mine or everybody's"
  // was exactly the question the old grid never answered. Keywords are the
  // words somebody would actually type when they cannot find a thing.
  const sections: PageSection[] = [
    {
      id: "appearance",
      title: "Appearance",
      group: "You",
      keywords: ["theme", "colors", "amber", "dark", "look"],
      render: () => <AppearanceSettings />,
    },
    {
      id: "notifications",
      title: "Notifications",
      group: "You",
      keywords: ["desktop", "chime", "sound", "alerts", "noise", "ping"],
      render: () => (
        <Panel title="Notifications">
          <NotificationSettings />
        </Panel>
      ),
    },
    {
      id: "email",
      title: "Email",
      group: "You",
      keywords: ["verify", "verification", "address", "invite"],
      render: () => (
        <Panel title="Email">
          <EmailVerification />
        </Panel>
      ),
    },
    {
      id: "security",
      title: "Security",
      group: "You",
      keywords: [
        "app password",
        "passkey",
        "vault",
        "lock",
        "unlock",
        "encrypt",
        "secrets",
        "webauthn",
        "biometric",
      ],
      render: () => (
        <Panel title="App password">
          <AppPasswordSettings />
        </Panel>
      ),
    },
    {
      id: "tokens",
      title: "Access tokens",
      group: "You",
      keywords: ["cli", "token", "nook login", "credential", "api key", "revoke"],
      render: () => (
        <Panel title="Access tokens">
          <AccessTokenSettings />
        </Panel>
      ),
    },
    {
      id: "git-credentials",
      title: "Git credentials",
      group: "Team",
      badge: "team",
      keywords: [
        "ssh",
        "key",
        "deploy key",
        "private repo",
        "clone",
        "credential",
        "fingerprint",
        "git",
      ],
      render: () => <GitCredentials />,
    },
    {
      id: "automation",
      title: "Automation",
      group: "Team",
      badge: "team",
      keywords: [
        "loops",
        "reconcile",
        "sessions",
        "dispatch",
        "jobs",
        "agents",
        "declarative",
        "converge",
        "queued",
      ],
      render: () => <AutomationSettings />,
    },
    {
      id: "channels",
      title: "Notification channels",
      group: "Team",
      badge: "team",
      keywords: ["ntfy", "webhook", "slack", "push", "phone", "channel"],
      render: () => <NotificationChannels />,
    },
    {
      id: "skills",
      title: "Taught skills",
      group: "Team",
      badge: "fleet",
      keywords: ["skill", "teach", "agents", "claude", "SKILL.md", "forget"],
      render: () => <TaughtSkillsSettings />,
    },
    {
      id: "visibility",
      title: "Operator visibility",
      group: "Team",
      badge: "org",
      keywords: ["operator", "privacy", "policy", "who can see", "metadata"],
      render: () => <OrgVisibility />,
    },
    {
      id: "instance",
      title: "About this instance",
      group: "Instance",
      keywords: ["docs", "api", "mcp", "add node", "version"],
      render: () => <InstanceInfo />,
    },
  ];

  return <SectionedPage sections={sections} placeholder="find a setting…" />;
}
