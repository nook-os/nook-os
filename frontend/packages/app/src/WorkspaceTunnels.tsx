// What this tenant has exposed to the outside, on the page that already holds
// what the repo binds (MAIN-510).
//
// The API shipped whole with MAIN-9/404 and nothing in `frontend/` had ever
// mentioned it, so a tunnel's URL — the entire product of opening one — was
// readable only from a terminal.
//
// Two things about the shape are worth knowing before reading further:
//
// `GET /api/v1/tunnels` is TENANT-scoped and `TunnelView` carries no workspace,
// so this cannot narrow to one repo without inventing a join the server does
// not make. Narrowing through `session_id` would be worse than not narrowing:
// a tunnel opened from THIS panel has no session at all, so the row somebody
// just created would vanish on the next poll. It lists what the endpoint
// returns and names the owning session when there is one.
//
// The panel is OFF, not empty, where the deployment has no `TUNNEL_DOMAIN` —
// which is the shipped default and production today. The list and the create
// call both answer 400 there, and the server's sentence already explains what
// is missing (a wildcard DNS record and a certificate), so it is surfaced
// verbatim rather than paraphrased into "no tunnels open" — which would be a
// lie about a feature that is not configured.
import React, { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Copy, Check } from "lucide-react";
import { api, type Schemas } from "@nookos/api";
import { Empty, Panel, Pill } from "@nookos/ui";
import { askConfirm } from "./dialogs";

type Tunnel = Schemas["TunnelView"];

/** The idle window the sweep uses, as the control plane ships it
 *  (`TUNNEL_IDLE_SECS`, `nook-infra/src/config.rs`).
 *
 *  It is deployment configuration and no endpoint publishes it, so the
 *  countdown below is against the default rather than against the number this
 *  particular deployment set. That is deliberately not a reason to print the
 *  raw idle seconds instead: "27m before it is swept" is right wherever the
 *  operator left the default and approximately right where they did not,
 *  whereas "180" is never an answer to the question anyone is asking. The
 *  label says "about" for the same reason. */
export const TUNNEL_IDLE_DEFAULT_SECS = 1800;

/** Under this much left, the row says so in warning colour — the case the
 *  field exists for, per its own type doc: seeing a tunnel about to be swept
 *  instead of discovering it gone. */
const URGENT_SECS = 300;

/** How long before the idle sweep closes it, in words (AC-4). */
export function sweepCountdown(
  idleSecs: number,
  windowSecs: number = TUNNEL_IDLE_DEFAULT_SECS,
): { text: string; urgent: boolean } {
  // `TUNNEL_IDLE_SECS=0` turns the sweep off entirely, so there is no
  // countdown to render and a "0m" would read as "about to go".
  if (windowSecs <= 0) return { text: "no idle sweep", urgent: false };
  const left = Math.round(windowSecs - idleSecs);
  if (left <= 0) return { text: "sweeping now", urgent: true };
  if (left < 60) return { text: "under a minute left", urgent: true };
  const mins = Math.floor(left / 60);
  if (mins < 60) return { text: `about ${mins}m left`, urgent: left <= URGENT_SECS };
  const hours = Math.floor(mins / 60);
  const rest = mins % 60;
  return { text: rest ? `about ${hours}h ${rest}m left` : `about ${hours}h left`, urgent: false };
}

/** The server's own words for a refusal — a 400 here names a missing piece of
 *  the deployment (`TUNNEL_DOMAIN`, DNS, a certificate) or the field that was
 *  just typed into, and no sentence guessed in the client beats either. */
export function refusalText(error: unknown, response?: Response): string {
  const e = error as { error?: string; message?: string } | undefined;
  if (typeof e?.error === "string") return e.error;
  if (typeof e?.message === "string") return e.message;
  if (error !== undefined && error !== null) return JSON.stringify(error);
  return response ? `${response.status} ${response.statusText}`.trim() : "the server refused";
}

export function WorkspaceTunnels({ workspaceId }: { workspaceId: string }) {
  const queryClient = useQueryClient();
  const [port, setPort] = useState("");
  const [nodeId, setNodeId] = useState("");
  const [busy, setBusy] = useState(false);
  const [refusal, setRefusal] = useState<string | null>(null);
  const [copied, setCopied] = useState<string | null>(null);

  // Both halves of the read in ONE result, because a refusal is not an empty
  // list and the two must never be collapsed by a `?? []` on the way out.
  const { data: listed } = useQuery({
    queryKey: ["tunnels"],
    queryFn: async () => {
      const { data, error, response } = await api.GET("/api/v1/tunnels");
      if (!response.ok) {
        return { tunnels: [] as Tunnel[], refusal: refusalText(error, response) };
      }
      return { tunnels: (data ?? []) as Tunnel[], refusal: null as string | null };
    },
    // The countdown moves and tunnels appear from sessions this page never
    // sees, so a static render would be stale within a minute.
    refetchInterval: 10000,
  });

  // Named owners for the `session_id` column. Scoped to this workspace by the
  // query — a tunnel from somewhere else in the tenant keeps its raw id rather
  // than shipping every session in the tenant to resolve one name.
  const { data: sessions } = useQuery({
    queryKey: ["sessions", workspaceId, "tunnels"],
    queryFn: async () =>
      (
        await api.GET("/api/v1/sessions", {
          params: { query: { workspace_id: workspaceId, active: true } },
        })
      ).data ?? [],
  });

  // Online only, like "clone to node…" on this page: the create call refuses a
  // node that is not connected, and offering one is offering a refusal.
  const { data: nodes } = useQuery({
    queryKey: ["nodes"],
    queryFn: async () => (await api.GET("/api/v1/nodes")).data ?? [],
  });
  const online = (nodes ?? []).filter((n) => n.status === "online");

  const off = listed?.refusal ?? null;
  const tunnels = listed?.tunnels ?? [];
  const sessionName = (id: string) => (sessions ?? []).find((s) => s.id === id)?.name ?? id;

  const copy = async (t: Tunnel) => {
    try {
      await navigator.clipboard.writeText(t.url);
      setCopied(t.label);
      window.setTimeout(() => setCopied(null), 1500);
    } catch {
      // Refusable on an insecure origin or by permission. The URL stays on
      // screen to take by hand, so nothing is lost by staying quiet.
    }
  };

  const open = async () => {
    // A user credential MUST name the machine — the API says so by name, and
    // there is no sensible default to pick on its behalf when a person can
    // reach several.
    if (!nodeId || !port) return;
    setBusy(true);
    setRefusal(null);
    const { error, response } = await api.POST("/api/v1/tunnels", {
      body: { port: Number(port), node_id: nodeId },
    });
    setBusy(false);
    if (error || !response.ok) {
      setRefusal(refusalText(error, response));
      return;
    }
    setPort("");
    queryClient.invalidateQueries({ queryKey: ["tunnels"] });
  };

  const stop = async (t: Tunnel) => {
    // Confirmed first: whatever is being served through that URL stops
    // answering the moment this returns, and the person on the other end of it
    // is not necessarily in this room.
    const ok = await askConfirm({
      title: `Stop tunnel ${t.label}?`,
      description: `${t.url} stops answering, and anything using it breaks.`,
      confirmLabel: "stop",
      danger: true,
    });
    if (!ok) return;
    setBusy(true);
    setRefusal(null);
    const { error, response } = await api.DELETE("/api/v1/tunnels/{label}", {
      params: { path: { label: t.label } },
    });
    setBusy(false);
    if (error || !response.ok) {
      setRefusal(refusalText(error, response));
      return;
    }
    queryClient.invalidateQueries({ queryKey: ["tunnels"] });
  };

  return (
    <Panel title="Tunnels">
      {off ? (
        // The whole panel, not a banner over an empty table: there is nothing
        // to list and nothing to open, and a create control here would only
        // collect the same 400.
        <div className="faint small" data-testid="tunnels-off">
          {off}
        </div>
      ) : (
        <div style={{ display: "flex", flexDirection: "column", gap: 10, minHeight: 0 }}>
          <div className="faint small">
            Every tunnel open in this tenant — the list the API keeps, which is not
            per-repo. A tunnel ends with the session that opened it, when it goes
            idle, or here.
          </div>

          {tunnels.length === 0 ? (
            <Empty>No tunnels are open.</Empty>
          ) : (
            <table className="nook-table">
              <thead>
                <tr>
                  <th>URL</th>
                  <th>Node</th>
                  <th>Port</th>
                  <th>Session</th>
                  <th>Idle sweep</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {tunnels.map((t) => {
                  const sweep = sweepCountdown(t.idle_secs);
                  return (
                    <tr key={t.label} data-testid={`tunnel-${t.label}`}>
                      <td className="mono">
                        <span>{t.url}</span>{" "}
                        <button
                          className="btn small icon"
                          aria-label={`copy ${t.label} URL`}
                          title="copy the URL"
                          onClick={() => copy(t)}
                        >
                          {copied === t.label ? <Check size={11} /> : <Copy size={11} />}
                        </button>
                      </td>
                      <td>{t.node_name}</td>
                      <td className="mono">{t.port}</td>
                      <td className="muted">{t.session_id ? sessionName(t.session_id) : "—"}</td>
                      <td>
                        <Pill
                          tone={sweep.urgent ? "warn" : "dim"}
                          title={`idle ${t.idle_secs}s · swept after ${TUNNEL_IDLE_DEFAULT_SECS}s idle unless this deployment set TUNNEL_IDLE_SECS`}
                        >
                          {sweep.text}
                        </Pill>
                      </td>
                      <td>
                        <button
                          className="btn small"
                          disabled={busy}
                          aria-label={`stop ${t.label}`}
                          onClick={() => stop(t)}
                        >
                          stop
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          )}

          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <input
              className="input small mono"
              type="number"
              min={1}
              max={65535}
              placeholder="3000"
              aria-label="port to expose"
              value={port}
              disabled={busy}
              onChange={(e) => setPort(e.target.value)}
            />
            <select
              className="input small"
              aria-label="machine to tunnel from"
              value={nodeId}
              disabled={busy}
              onChange={(e) => setNodeId(e.target.value)}
            >
              <option value="">choose a machine…</option>
              {online.map((n) => (
                <option key={n.id} value={n.id}>
                  {n.name}
                </option>
              ))}
            </select>
            <button
              className="btn primary small"
              disabled={busy || !nodeId || !port}
              onClick={open}
            >
              open tunnel
            </button>
            <span className="faint small">
              {online.length === 0
                ? "no node is online to tunnel from"
                : !nodeId
                  ? "say which machine to tunnel from"
                  : "the port is the one on that machine's own loopback"}
            </span>
          </div>

          {refusal && (
            <div className="small" data-testid="tunnels-refusal" style={{ color: "var(--nook-err)" }}>
              {refusal}
            </div>
          )}
        </div>
      )}
    </Panel>
  );
}
