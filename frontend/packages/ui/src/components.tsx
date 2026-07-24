import React from "react";

export function Panel({
  title,
  actions,
  children,
  style,
  className = "",
}: {
  title?: React.ReactNode;
  actions?: React.ReactNode;
  children: React.ReactNode;
  style?: React.CSSProperties;
  /** For panels that manage their own scrolling — see `.git-panel`, whose
   *  commit bar stays put while only the diff moves. The body scrolls by
   *  default, which is right for nearly every panel. */
  className?: string;
}) {
  return (
    <section className={`nook-panel ${className}`.trim()} style={style}>
      {title !== undefined && (
        <header className="nook-panel-title">
          {/* Two stable classes, not :first/:last-child: a panel may have a
              title and no actions, where one span would be BOTH — so the title
              truncates and the actions never shrink, unambiguously (MAIN-47). */}
          <span className="nook-panel-heading">{title}</span>
          {actions && <span className="nook-panel-actions">{actions}</span>}
        </header>
      )}
      <div className="nook-panel-body">{children}</div>
    </section>
  );
}

/**
 * The app's whole color vocabulary. Colour carries meaning here, so a tone is a
 * claim about *what a thing is*, not just how it looks — pick by role, never by
 * shade you happen to want:
 *
 *   accent  identity — a name, a runtime, a version. The amber the brand runs on.
 *   ok      healthy / done / secured — online, running, clean, sealed.
 *   warn    needs attention, not broken — starting, detached, dirty, ephemeral.
 *   err     wrong — offline, exited, error, blocked.
 *   info    reference metadata — a git branch, a worktree count, a classification.
 *   dim     chrome — secondary context that shouldn't compete for the eye.
 *
 * A name shown as `ok` (green) is the classic slip: green reads as "healthy",
 * which says nothing true about a name. Names are `accent`. See `statusTone`
 * below for the status→tone mapping every status pill should route through.
 */
export type Tone = "ok" | "warn" | "err" | "info" | "accent" | "dim";

export function Pill({
  tone,
  children,
  title,
}: {
  tone?: Tone;
  children: React.ReactNode;
  /** Hover text. A pill compresses a state into a word; this is where the
      sentence explaining it goes, without spending a row on it. */
  title?: string;
}) {
  return (
    <span className={`pill ${tone ?? ""}`} title={title}>
      {children}
    </span>
  );
}

export function StatusDot({ status }: { status: string }) {
  const cls =
    status === "online" || status === "running"
      ? "ok"
      : status === "offline" || status === "exited" || status === "error"
        ? "err"
        : "dim";
  return <span className={`dot ${cls}`} title={status} />;
}

export function Empty({ children }: { children: React.ReactNode }) {
  return <div className="empty">{children}</div>;
}

function gb(bytes: number): string {
  return (bytes / 1024 / 1024 / 1024).toFixed(1);
}

function fillClass(pct: number): string {
  return pct >= 90 ? "err" : pct >= 70 ? "warn" : "";
}

/** Live node capacity bars — so you can see which machine can take the work. */
export function ResourceBars({ resources }: { resources: unknown }) {
  const r = (resources ?? {}) as {
    cpu_percent?: number;
    mem_used?: number;
    mem_total?: number;
    load_avg1?: number;
    active_sessions?: number;
  };
  // Offline nodes have no sample, and collapsing to a single line made their
  // rows half the height of a reporting node's — which is what made the table
  // look ragged rather than misaligned. Reserve the same space either way.
  if (r.mem_total === undefined && r.cpu_percent === undefined) {
    return (
      <div className="res-empty">
        <span className="faint small">no sample yet</span>
      </div>
    );
  }
  const cpu = Math.round(r.cpu_percent ?? 0);
  const memPct =
    r.mem_total && r.mem_used ? Math.round((r.mem_used / r.mem_total) * 100) : 0;
  return (
    <div className="res-bars">
      <div className="res-bar">
        <span className="label">cpu</span>
        <span className="track">
          <span className={`fill ${fillClass(cpu)}`} style={{ width: `${cpu}%` }} />
        </span>
        <span className="val">{cpu}%</span>
      </div>
      <div className="res-bar">
        <span className="label">mem</span>
        <span className="track">
          <span className={`fill ${fillClass(memPct)}`} style={{ width: `${memPct}%` }} />
        </span>
        <span className="val">
          {gb(r.mem_used ?? 0)}/{gb(r.mem_total ?? 0)}G
        </span>
      </div>
      <div className="res-bar">
        <span className="label">load</span>
        <span className="val" style={{ width: "auto", textAlign: "left" }}>
          {(r.load_avg1 ?? 0).toFixed(2)} · {r.active_sessions ?? 0} sessions
        </span>
      </div>
    </div>
  );
}

export function statusTone(status: string): Tone {
  switch (status) {
    case "online":
    case "running":
      return "ok";
    case "starting":
    case "detached":
    case "reconnecting":
      return "warn";
    case "offline":
    case "exited":
    case "error":
      return "err";
    default:
      return "dim";
  }
}
