import React from "react";
import {
  BookOpen,
  Bug,
  Building2,
  Layers,
  Lock,
  SquareCheck,
  Users,
  Wrench,
  type LucideIcon,
} from "lucide-react";

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

/** The five issue types (MAIN-59). String, not an enum, because the server is
 *  the source of truth for the set; this is only its presentation. */
export type TaskType = "task" | "bug" | "epic" | "story" | "chore";

export interface TypeMeta {
  value: TaskType;
  label: string;
  tone: Tone;
  Icon: LucideIcon;
}

/**
 * The ONE issue-type → (tone, icon, label) mapping (AC-2). The task-detail
 * selector, the board card, and the board filter all read it, so a type looks
 * identical everywhere and a new type is added in exactly one place. Tones come
 * from the shared vocabulary above — no per-type colours (NG-5), no raw emoji.
 */
export const TYPE_META: TypeMeta[] = [
  { value: "task", label: "Task", tone: "dim", Icon: SquareCheck },
  { value: "bug", label: "Bug", tone: "err", Icon: Bug },
  { value: "epic", label: "Epic", tone: "accent", Icon: Layers },
  { value: "story", label: "Story", tone: "info", Icon: BookOpen },
  { value: "chore", label: "Chore", tone: "dim", Icon: Wrench },
];

/** Look up a type's presentation, defaulting to `task` for an absent or unknown
 *  value — the server's own default — so nothing ever renders blank. */
export function typeMeta(type: string | null | undefined): TypeMeta {
  return TYPE_META.find((t) => t.value === type) ?? TYPE_META[0];
}

/**
 * A compact, theme-native indicator for an issue type: the type's icon, plus
 * its label unless `compact`. Toned from the shared vocabulary. Reused on board
 * cards (compact), in the detail selector, and in the board filter (AC-2/AC-3).
 */
export function TypeBadge({
  type,
  compact = false,
}: {
  type: string | null | undefined;
  compact?: boolean;
}) {
  const m = typeMeta(type);
  return (
    <span className={`type-badge ${m.tone}`} title={m.label}>
      <m.Icon size={12} className="type-badge-icon" />
      {!compact && <span className="type-badge-label">{m.label}</span>}
    </span>
  );
}

/** The three per-task visibilities (MAIN-103). String, not an enum, because the
 *  server owns the set; this is only its presentation. Default is `team`. */
export type TaskVisibility = "private" | "team" | "org";

export interface VisibilityMeta {
  value: TaskVisibility;
  label: string;
  tone: Tone;
  /** The sentence the badge/tooltip carries — who this card is visible to. */
  tooltip: string;
  Icon: LucideIcon;
}

/**
 * The ONE visibility → (tone, icon, label, tooltip) mapping (MAIN-103), mirroring
 * `TYPE_META`. The detail selector, the board card, the context menu and the
 * board filter all read it, so a visibility looks identical everywhere and a new
 * value is added in exactly one place. Tones come from the shared vocabulary:
 * `private` is restricted (warn), `team` is the quiet default (dim), `org` is
 * reference-wide (info).
 */
export const VISIBILITY_META: VisibilityMeta[] = [
  {
    value: "private",
    label: "Private",
    tone: "warn",
    tooltip: "Private — only the creator and assignee can see this card.",
    Icon: Lock,
  },
  {
    value: "team",
    label: "Team",
    tone: "dim",
    tooltip: "Team — visible to the whole tenant (the default).",
    Icon: Users,
  },
  {
    value: "org",
    label: "Org",
    tone: "info",
    tooltip: "Org — visible across the organization.",
    Icon: Building2,
  },
];

/** Look up a visibility's presentation, defaulting to `team` for an absent or
 *  unknown value — the server's own default — so nothing ever renders blank. */
export function visibilityMeta(visibility: string | null | undefined): VisibilityMeta {
  return VISIBILITY_META.find((v) => v.value === visibility) ?? VISIBILITY_META[1];
}

/**
 * A compact, theme-native indicator for a task's visibility: the icon, plus its
 * label unless `compact`. Same props/shape/styling as `TypeBadge` — it reuses
 * the `type-badge` classes so a visibility badge sits identically beside a type
 * badge. Reused on board cards (compact) and in the board filter (MAIN-103).
 */
export function VisibilityBadge({
  visibility,
  compact = false,
}: {
  visibility: string | null | undefined;
  compact?: boolean;
}) {
  const m = visibilityMeta(visibility);
  return (
    <span className={`type-badge ${m.tone}`} title={m.tooltip}>
      <m.Icon size={12} className="type-badge-icon" />
      {!compact && <span className="type-badge-label">{m.label}</span>}
    </span>
  );
}

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

type DiskSample = {
  label?: string;
  mount_point?: string;
  free_bytes?: number;
  total_bytes?: number;
};

/** Live node capacity bars — so you can see which machine can take the work. */
export function ResourceBars({ resources }: { resources: unknown }) {
  const r = (resources ?? {}) as {
    cpu_percent?: number;
    mem_used?: number;
    mem_total?: number;
    load_avg1?: number;
    active_sessions?: number;
    disks?: DiskSample[];
    disk_shortage?: string | null;
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
  // The TIGHTEST filesystem the node samples (MAIN-618): it is the one that
  // decides whether the machine takes loop work, and the roomy one cannot lift
  // a gate it imposed. Absent for an agent that predates the field, which draws
  // no bar rather than an empty one.
  const disk = (r.disks ?? []).reduce<DiskSample | undefined>(
    (tightest, d) =>
      tightest === undefined || (d.free_bytes ?? 0) < (tightest.free_bytes ?? 0) ? d : tightest,
    undefined,
  );
  const diskPct =
    disk?.total_bytes && disk.free_bytes !== undefined
      ? Math.round(((disk.total_bytes - disk.free_bytes) / disk.total_bytes) * 100)
      : 0;
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
      {disk && (
        <div className="res-bar" title={r.disk_shortage ?? disk.label ?? ""}>
          <span className="label">disk</span>
          <span className="track">
            <span
              className={`fill ${r.disk_shortage ? "err" : fillClass(diskPct)}`}
              style={{ width: `${diskPct}%` }}
            />
          </span>
          <span className="val">
            {gb(disk.free_bytes ?? 0)}G free{r.disk_shortage ? " · low" : ""}
          </span>
        </div>
      )}
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
    // Intentional and resumable, so NOT `err` (MAIN-415 AC-6). A stopped
    // session reading the same red as a crashed one is the whole distinction
    // this state exists to make, thrown away at the last step.
    case "stopped":
      return "dim";
    case "offline":
    case "exited":
    case "error":
      return "err";
    default:
      return "dim";
  }
}
