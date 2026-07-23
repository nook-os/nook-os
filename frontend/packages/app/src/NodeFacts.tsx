// What a machine is, rendered as facts rather than as its wire format.
//
// The node detail page printed `JSON.stringify(capabilities, null, 2)` into a
// `<pre>`. That is the shape the data arrives in, not the shape a person reads:
// memory as `51539607552`, an SSH key wrapping across eight lines, and the one
// field anybody actually wants — which version of the agent is running —
// indistinguishable from the fifteen around it.
//
// The version got a column of its own because it answers a question the JSON
// could not: is this machine running what the control plane expects? That
// comparison needs two values, and only one of them is in `capabilities`.
import React from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "@nookos/api";
import { Pill } from "@nookos/ui";

/**
 * The agent version this control plane expects every node to run.
 *
 * The same string it puts in `RegisterAck`, read from the same endpoint the
 * node's own updater uses — so the UI shows the comparison the node makes,
 * not a second opinion that could disagree with it.
 */
export function useControlPlaneVersion(): string | undefined {
  const { data } = useQuery({
    queryKey: ["node-releases"],
    queryFn: async () => (await api.GET("/api/v1/node/releases")).data ?? null,
    staleTime: 5 * 60 * 1000,
    retry: false,
  });
  return data?.version;
}

/**
 * A node's agent version, against what the control plane expects.
 *
 * Three states worth telling apart, because the action differs for each:
 *
 * - **matching** — nothing to do.
 * - **behind/ahead** — it will update itself on its next reconnect, provided
 *   it runs under a service manager.
 * - **unknown** — the agent is too old to report a version at all. Self-update
 *   shipped in 0.4.3, so an agent that does not say what it is also cannot
 *   update itself, and somebody has to go and do it once by hand. That is the
 *   opposite of "probably fine", which is what a blank cell would imply.
 */
export function AgentVersion({
  reported,
  expected,
}: {
  reported?: string | null;
  expected?: string;
}) {
  if (!reported) {
    return (
      <Pill tone="warn" title="This agent predates version reporting (0.4.3). It cannot update itself — install the new binary on that machine once, and it will keep itself current after that.">
        unknown · pre-0.4.3
      </Pill>
    );
  }
  if (!expected || reported === expected) {
    return <span className="mono muted">{reported}</span>;
  }
  return (
    <Pill
      tone="warn"
      title={`This control plane expects ${expected}. A supervised agent updates itself on its next reconnect.`}
    >
      {reported} → {expected}
    </Pill>
  );
}

/** `51539607552` is not a memory size anybody reads. */
function bytes(n: unknown): string {
  const v = typeof n === "number" ? n : NaN;
  if (!Number.isFinite(v) || v <= 0) return "—";
  const gb = v / 1024 ** 3;
  return gb >= 1024 ? `${(gb / 1024).toFixed(1)} TB` : `${Math.round(gb)} GB`;
}

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <tr>
      <td className="faint small" style={{ width: 130, verticalAlign: "top" }}>
        {label}
      </td>
      <td className="small" style={{ wordBreak: "break-word" }}>
        {children}
      </td>
    </tr>
  );
}

/**
 * Everything a node reports about itself, as a table.
 *
 * Only the fields worth a person's attention are named. Anything the node
 * reports that this does not know about is still shown, at the bottom, rather
 * than dropped — a capability nobody rendered is exactly the one you go looking
 * for when something does not work.
 */
export function NodeFacts({
  node,
}: {
  node: {
    id: string;
    name: string;
    hostname: string;
    platform: string;
    status: string;
    last_seen_at?: string | null;
    capabilities: unknown;
  };
}) {
  const caps = (node.capabilities ?? {}) as Record<string, unknown>;
  const expected = useControlPlaneVersion();

  // Named above, so they are not repeated in the "also reported" list. The SSH
  // key has its own panel and the resource bars cover the live numbers.
  const known = new Set([
    "agent_version",
    "cpus",
    "memory",
    "gpus",
    "runtimes",
    "platform",
    "architecture",
    "hostname",
    "tmux",
    "docker",
    "git",
    "ssh_public_key",
  ]);
  const extra = Object.entries(caps).filter(([k]) => !known.has(k));

  const gpus = (caps.gpus as { model?: string }[] | undefined) ?? [];
  const runtimes = (caps.runtimes as string[] | undefined) ?? [];

  return (
    <table className="nook-table" style={{ tableLayout: "fixed" }}>
      <tbody>
        <Row label="Agent">
          <AgentVersion
            reported={caps.agent_version as string | null}
            expected={expected}
          />
        </Row>
        <Row label="Node ID">
          {/* `user-select: all` so one click grabs the whole id — it is what
              every CLI command about this machine wants as an argument. */}
          <span className="mono" style={{ userSelect: "all" }}>
            {node.id}
          </span>
        </Row>
        <Row label="Hostname">
          <span className="mono">{(caps.hostname as string) ?? node.hostname}</span>
        </Row>
        <Row label="Platform">
          {node.platform}
          {caps.architecture ? ` · ${caps.architecture}` : ""}
        </Row>
        <Row label="CPU / memory">
          {(caps.cpus as number) ?? "—"} cores · {bytes(caps.memory)}
        </Row>
        <Row label="GPU">
          {gpus.length ? gpus.map((g) => g.model ?? "gpu").join(", ") : "—"}
        </Row>
        <Row label="Runtimes">
          {runtimes.length
            ? runtimes.map((r) => <Pill key={r}>{r}</Pill>)
            : "—"}
        </Row>
        <Row label="Tooling">
          <Pill tone={caps.tmux ? "ok" : "warn"}>
            tmux {caps.tmux ? "yes" : "missing"}
          </Pill>{" "}
          <Pill tone={caps.docker ? "ok" : undefined}>
            docker {caps.docker ? "yes" : "no"}
          </Pill>{" "}
          <Pill>git {(caps.git as string) ?? "—"}</Pill>
        </Row>
        <Row label="Last seen">
          {node.last_seen_at
            ? new Date(node.last_seen_at).toLocaleString()
            : "never"}
        </Row>
        {extra.map(([k, v]) => (
          <Row key={k} label={k}>
            <span className="mono muted">
              {typeof v === "object" ? JSON.stringify(v) : String(v)}
            </span>
          </Row>
        ))}
      </tbody>
    </table>
  );
}
