// Opening a terminal in a workspace, from wherever you happen to be.
//
// Killing a managed session removes it for good — the reconciler does not bring
// it back, which is the decision MAIN-324 made deliberately (a tmux session is
// not the checkout the declaration is about). That is fine only if starting
// another one is trivial, and it was not: every path led to the full New Work
// modal, which is a clone/worktree form, not a "give me a shell" button.
//
// The rule, in one place because the tab strip's `+` and the navigator's
// right-click both need it and must not answer differently:
//
//   one live checkout  → open a terminal there, no questions
//   several            → New Work, seeded with the workspace, so you choose
//   none               → New Work, because there is nowhere to put a terminal
//                        until the repo is cloned somewhere
//
// Escalating to the modal rather than guessing a node is the point: picking
// "the first checkout" out of four machines is the kind of helpfulness that
// puts your shell on the wrong host.
import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "react-router-dom";
import { api, type Schemas } from "@nookos/api";
import { notify } from "./dialogs";
import { useNewWork, type NewWorkSeed } from "./newwork";

// `WorkspaceDetail`, not `Workspace`: the bare row has no checkouts, and the
// checkouts are the entire question this module answers.
type Workspace = Schemas["WorkspaceDetail"];
type Location = Schemas["WorkspaceLocation"];

/** Which checkout a terminal should open in, or why we cannot decide.
 *
 *  Pure, so the rule is testable without a control plane or a DOM. An offline
 *  node is not a candidate: the session would be created against a machine
 *  that cannot start it, and the failure would arrive as a mystery later
 *  rather than as a choice now.
 */
export function terminalTarget(locations: Location[]): {
  kind: "one" | "choose" | "none";
  location?: Location;
} {
  const live = locations.filter((l) => l.node_status === "online");
  if (live.length === 1) return { kind: "one", location: live[0] };
  if (live.length > 1) return { kind: "choose" };
  return { kind: "none" };
}

export function useNewTerminal() {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const showNewWork = useNewWork((s) => s.show);

  /** Open a terminal in `ws`, or escalate to New Work when there is a real
   *  choice to make. `seed` lets a caller add context to that escalation. */
  return async (ws: Workspace, seed: NewWorkSeed = {}) => {
    const target = terminalTarget(ws.locations ?? []);
    if (target.kind !== "one" || !target.location) {
      showNewWork({ ...seed, workspaceId: ws.id });
      return;
    }

    const { data: session, error, response } = await api.POST("/api/v1/sessions", {
      body: {
        workspace_id: ws.id,
        node_id: target.location.node_id,
        runtime: "bash",
        path: target.location.path,
      },
    });
    if (error || !response.ok) {
      await notify(
        "Could not open a terminal",
        error ? String((error as { error: unknown }).error) : response.statusText,
      );
      return;
    }
    // The strip reads `["sessions", "tabs"]`; the navigator and Mission read
    // their own. Invalidate broadly rather than naming them — a new session is
    // exactly the event every session surface wants.
    await queryClient.invalidateQueries({ queryKey: ["sessions"] });
    if (session?.id) navigate(`/sessions/${session.id}`);
  };
}
