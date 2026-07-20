// Desktop notifications + an audible chime for workflow events.
//
// Driven off the live activity stream: when something you'd want to look up
// for happens (a clone finished, an agent session exited, a node dropped),
// the browser says so even if NookOS isn't the focused tab. Preferences are
// local to the browser; the chime is synthesized with WebAudio so there's no
// binary asset in the repo.
import { create } from "zustand";
import { persist } from "zustand/middleware";
import type { EventItem } from "@nookos/api";

interface NotifyState {
  /** Show desktop notifications (requires browser permission). */
  desktop: boolean;
  /** Play a chime. */
  sound: boolean;
  /** Notify for everything, not just the curated "worth interrupting" set. */
  everything: boolean;
  set(patch: Partial<Omit<NotifyState, "set">>): void;
}

export const useNotify = create<NotifyState>()(
  persist(
    (set) => ({
      desktop: false,
      sound: true,
      everything: false,
      set: (patch) => set(patch),
    }),
    { name: "nookos-notifications" },
  ),
);

/** Events worth interrupting someone for, with how to phrase them. */
const INTERESTING: Record<string, { title: string; tone: Tone }> = {
  "git.clone_finished": { title: "Clone finished", tone: "ok" },
  "workspace.project_created": { title: "Project created", tone: "ok" },
  "workspace.worktree_added": { title: "Worktree ready", tone: "ok" },
  "node.connected": { title: "Node connected", tone: "ok" },
  "node.joined": { title: "Node joined", tone: "ok" },
  "node.disconnected": { title: "Node disconnected", tone: "warn" },
  "node.error": { title: "Node error", tone: "warn" },
  // Both ends of a session's life. Starting one is the more surprising event
  // of the two once agents can do it for you: something began running on one
  // of your machines, and you didn't press anything.
  "session.created": { title: "Session started", tone: "ok" },
  "session.exited": { title: "Session ended", tone: "warn" },
  "task.work_started": { title: "Work started", tone: "ok" },
  "task.pr_submitted": { title: "PR submitted", tone: "ok" },
  "task.dispatched": { title: "Task dispatched", tone: "ok" },
  "user.login": { title: "User signed in", tone: "ok" },
};

type Tone = "ok" | "warn";

export function isInteresting(kind: string): boolean {
  return kind in INTERESTING;
}

/** Ask the browser for notification permission (must be user-initiated). */
export async function requestDesktopPermission(): Promise<boolean> {
  if (typeof Notification === "undefined") return false;
  if (Notification.permission === "granted") return true;
  if (Notification.permission === "denied") return false;
  return (await Notification.requestPermission()) === "granted";
}

export function desktopPermission(): string {
  return typeof Notification === "undefined" ? "unsupported" : Notification.permission;
}

let audioCtx: AudioContext | null = null;

/** Two-tone chime — rising for good news, falling for the "look at me" kind. */
export function playChime(tone: Tone = "ok") {
  try {
    const Ctor =
      window.AudioContext ??
      (window as unknown as { webkitAudioContext?: typeof AudioContext })
        .webkitAudioContext;
    if (!Ctor) return;
    audioCtx ??= new Ctor();
    const ctx = audioCtx;
    // Autoplay policy: the context starts suspended until a user gesture.
    if (ctx.state === "suspended") void ctx.resume();

    const steps = tone === "ok" ? [660, 880] : [560, 420];
    const now = ctx.currentTime;
    steps.forEach((freq, i) => {
      const osc = ctx.createOscillator();
      const gain = ctx.createGain();
      osc.type = "sine";
      osc.frequency.value = freq;
      const at = now + i * 0.11;
      // Short pluck: quick attack, exponential tail — no clicks.
      gain.gain.setValueAtTime(0.0001, at);
      gain.gain.exponentialRampToValueAtTime(0.09, at + 0.012);
      gain.gain.exponentialRampToValueAtTime(0.0001, at + 0.19);
      osc.connect(gain).connect(ctx.destination);
      osc.start(at);
      osc.stop(at + 0.22);
    });
  } catch {
    // Audio unavailable — notifications still work.
  }
}

/** A short human phrase for an activity event. */
export function describe(event: EventItem): { title: string; body: string; tone: Tone } {
  const meta = INTERESTING[event.kind];
  const payload = (event.payload ?? {}) as Record<string, unknown>;
  const detail =
    (typeof payload.name === "string" && payload.name) ||
    (typeof payload.title === "string" && payload.title) ||
    (typeof payload.message === "string" && payload.message) ||
    (typeof payload.branch === "string" && payload.branch) ||
    "";
  return {
    title: meta?.title ?? event.kind.replace(/[._]/g, " "),
    body: detail || event.kind,
    tone: meta?.tone ?? "ok",
  };
}

/** Fire the configured notifications for an activity event. */
export function notifyEvent(event: EventItem) {
  const { desktop, sound, everything } = useNotify.getState();
  if (!everything && !isInteresting(event.kind)) return;
  const { title, body, tone } = describe(event);

  if (sound) playChime(tone);
  if (desktop && typeof Notification !== "undefined" && Notification.permission === "granted") {
    try {
      // tag+renotify collapses repeats of the same kind instead of stacking.
      new Notification(`NookOS · ${title}`, {
        body,
        tag: event.kind,
        icon: "/favicon.png",
      });
    } catch {
      // Some browsers refuse construction outside a service worker.
    }
  }
}
