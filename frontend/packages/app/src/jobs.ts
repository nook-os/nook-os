// Background work the user should be able to walk away from.
//
// Long operations (cloning a big repo) run on the control plane and report
// completion through the activity stream. A job here is the local half: what
// was started, what it's called, and whether we've heard back — so the UI can
// show progress without holding a modal open.
import { create } from "zustand";
import { persist } from "zustand/middleware";

export type JobState = "running" | "done" | "failed";

export interface Job {
  id: string;
  /** What's happening, e.g. "Cloning acme/services". */
  label: string;
  /** Short kind for the icon: clone | worktree | project | generic. */
  kind: string;
  state: JobState;
  startedAt: number;
  finishedAt?: number;
  message?: string;
  /** Where to go when the job finishes (e.g. the new workspace). */
  href?: string;
}

interface JobsState {
  jobs: Job[];
  start(job: Omit<Job, "state" | "startedAt">): void;
  finish(id: string, ok: boolean, message?: string, href?: string): void;
  dismiss(id: string): void;
  clearFinished(): void;
}

export const useJobs = create<JobsState>((set) => ({
  jobs: [],
  start: (job) =>
    set((s) => ({
      jobs: [
        ...s.jobs.filter((j) => j.id !== job.id),
        { ...job, state: "running", startedAt: Date.now() },
      ],
    })),
  finish: (id, ok, message, href) =>
    set((s) => ({
      jobs: s.jobs.map((j) =>
        j.id === id
          ? {
              ...j,
              state: ok ? "done" : "failed",
              finishedAt: Date.now(),
              message,
              href: href ?? j.href,
            }
          : j,
      ),
    })),
  dismiss: (id) => set((s) => ({ jobs: s.jobs.filter((j) => j.id !== id) })),
  clearFinished: () =>
    set((s) => ({ jobs: s.jobs.filter((j) => j.state === "running") })),
}));

/** Where the floating panel sits — dragged by the user, remembered. */
interface HudPosition {
  x: number | null;
  y: number | null;
  set(x: number, y: number): void;
}

export const useHudPosition = create<HudPosition>()(
  persist(
    (set) => ({
      x: null,
      y: null,
      set: (x, y) => set({ x, y }),
    }),
    { name: "nookos-hud-position" },
  ),
);

/// What to do once a background job lands — e.g. start the session the user
/// actually asked for after a clone finishes. Kept out of the store because
/// these are closures, not state: they must not be persisted or serialized.
const followUps = new Map<string, (ok: boolean) => void | Promise<void>>();

export function onJobFinish(
  id: string,
  fn: (ok: boolean) => void | Promise<void>,
): void {
  followUps.set(id, fn);
}

/** Run and forget a job's follow-up. Safe to call for jobs that have none. */
export function runJobFollowUp(id: string, ok: boolean): void {
  const fn = followUps.get(id);
  followUps.delete(id);
  if (fn) void fn(ok);
}
