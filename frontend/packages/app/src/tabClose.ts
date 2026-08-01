// What closing a tab MEANS, per session type (MAIN-324).
//
// The strip is a view of the live sessions (MAIN-322), so there is no local
// list to drop a tab from: closing can only end the session. And ending it
// means two different things.
//
// An AD-HOC session is somebody's terminal. Killing it is the whole of closing
// it, and it is destructive, so it is confirmed and offers a reboot afterwards.
//
// A MANAGED session is one the reconciler started for its workspace's
// SessionSpec. Killing it is NOT closing it: the next reconcile pass sees a
// checkout with no live managed session and starts another (MAIN-318). Closing
// one means editing the declaration — lower the replicas — which is why the
// managed branch never offers a kill and says why.
//
// The decision is a pure function so the semantics can be tested without a
// browser or a control plane. Getting this wrong is not a cosmetic bug: it is
// either a terminal killed without asking, or a "close" that visibly does
// nothing thirty seconds later.
import type { SessionTab } from "./sessionTabsStore";

export type ClosePlan =
  | {
      kind: "kill";
      title: string;
      description: string;
      confirmLabel: string;
    }
  | {
      kind: "scale-down";
      workspaceId: string;
      title: string;
      description: string;
      confirmLabel: string;
    }
  | {
      // Managed, but we cannot name the workspace to scale down — an
      // inconsistency rather than a user error, so it explains instead of
      // offering a kill that would respawn.
      kind: "explain";
      title: string;
      description: string;
    };

export function closePlan(tab: SessionTab): ClosePlan {
  if (!tab.managed) {
    return {
      kind: "kill",
      title: `Close ${tab.name}?`,
      description:
        "This ends the session and everything running in it. Nothing restarts it — " +
        "you can reboot it from the notice afterwards if this was a mistake.",
      confirmLabel: "close and end",
    };
  }
  if (!tab.workspaceId) {
    return {
      kind: "explain",
      title: `${tab.name} is a managed session`,
      description:
        "The reconciler started it for a workspace declaration, so killing it only " +
        "pauses it — the next pass starts another. It should be closed by lowering " +
        "that workspace's replicas, but this tab is not showing which workspace it " +
        "belongs to, so there is nothing safe to change from here.",
    };
  }
  return {
    kind: "scale-down",
    workspaceId: tab.workspaceId,
    title: `Scale down ${tab.workspaceName ?? "this workspace"}?`,
    description:
      `${tab.name} is MANAGED — the reconciler keeps it running for this ` +
      "workspace's declaration. Killing it would not close it: the next pass " +
      "would start another. Closing it means asking for one fewer session, " +
      "which lowers the workspace's replicas for everyone.",
    confirmLabel: "scale down",
  };
}

/** The next `replicas` after scaling down by one, or `null` when there is
 *  nothing left to give up.
 *
 *  `all` cannot be decremented — it means "one on every matching node", so the
 *  honest answer is a count one below however many are running now, which the
 *  caller supplies. `single` scaling down is zero: managed-and-wanting-none is
 *  expressible on purpose (MAIN-315) and is what "close the last one" means. */
export function scaledDown(
  replicas: { kind: string; count?: number } | undefined,
  running: number,
): { kind: "count"; count: number } | null {
  if (!replicas) return null;
  if (replicas.kind === "count") {
    const next = (replicas.count ?? 0) - 1;
    return next >= 0 ? { kind: "count", count: next } : null;
  }
  if (replicas.kind === "single") return { kind: "count", count: 0 };
  if (replicas.kind === "all") {
    const next = running - 1;
    return next >= 0 ? { kind: "count", count: next } : null;
  }
  return null;
}
