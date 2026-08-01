// A workspace that has not said what it binds (MAIN-361), and the ticket that
// gets it fixed.
//
// A workspace with no port declaration gets no variables its app reads, so the
// app binds whatever it hardcoded. Start a second session and both bind 3000;
// the second fails in a way that looks like the app's fault rather than nook's.
// So the reconciler holds such a workspace to one session per node, and this is
// the part that explains it and offers a route out.
//
// NOOK DOES NOT READ THE REPO. "Has a declaration" is the whole detection
// (NG-2) — no scanning for port literals, no guessing what an app needs, and
// certainly no editing its source (NG-1). The route out is a ticket a builder
// picks up as ordinary loop work.
import { api, type Board } from "@nookos/api";
import { boardForWorkspace } from "./newspec";
import { notify } from "./dialogs";

/** The title the dedup match keys on, so filing twice finds the first one.
 *
 *  A constant rather than a formatted string: the search below is a substring
 *  query, and a title carrying the workspace name would stop matching the moment
 *  somebody renamed the workspace. */
export const PORTS_TICKET_TITLE = "Declare this repo's ports";

/** Whether the reconciler is holding this workspace to one session per node.
 *
 *  Derived from what the server derived — never stored on either side, so a
 *  declaration landing lifts it on the next read with nothing to invalidate. */
export function isPortCapped(status: { port_capped?: boolean } | null | undefined): boolean {
  return !!status?.port_capped;
}

/** The ticket body, written for a builder who has no other context.
 *
 *  It names the repo, says what to add, and — the part that actually matters —
 *  says the app has to READ those variables. A declaration nobody reads changes
 *  nothing: the ports would be leased and the app would still bind 3000. */
export function portsTicketBody(workspaceName: string): string {
  return [
    `\`${workspaceName}\` declares no ports, so nook is holding it to ONE session per node.`,
    "",
    "Two sessions of this repo on one machine would both bind whatever the app",
    "hardcodes, and the second would fail in a way that looks like the app's fault.",
    "",
    "## What to do",
    "",
    "1. Add a `.nook.toml` at the repo root declaring each listener it binds:",
    "",
    "   ```toml",
    "   [[ports]]",
    '   name = "web"',
    '   env  = "PORT"',
    "   ```",
    "",
    "2. Change the app to read those variables instead of its hardcoded ports —",
    "   this is the half that matters. A declaration the app ignores leases a",
    "   port and changes nothing.",
    "",
    "3. If this repo genuinely binds nothing, declare that instead: an empty",
    "   `[[ports]]` list is a valid statement and lifts the cap just as well.",
    "",
    "The cap lifts by itself once a declaration exists — nothing here needs",
    "closing, and closing it without declaring will not lift it.",
  ].join("\n");
}

/** File the ticket, or surface the one already filed (AC-7).
 *
 *  Returns the task key either way, so the caller can link to it without caring
 *  which happened. Deliberately NOT automatic and deliberately without
 *  `agent-ready` (NG-3): nook filing its own work must not also walk it through
 *  the human approval gate.
 */
export async function fileOrFindPortsTicket(
  workspaceId: string,
  workspaceName: string,
): Promise<{ key: string; existed: boolean } | null> {
  // Look first. Scoped to this workspace so an identically-titled ticket on
  // another repo is not mistaken for this one's.
  const found = (
    await api.GET("/api/v1/tasks", {
      params: { query: { workspace_id: workspaceId, q: PORTS_TICKET_TITLE } },
    })
  ).data;
  const existing = (found ?? []).find((t) => t.title === PORTS_TICKET_TITLE && t.key);
  if (existing?.key) return { key: existing.key, existed: true };

  const boards = ((await api.GET("/api/v1/boards")).data ?? []) as Board[];
  const board = boardForWorkspace(boards, workspaceId);
  if (!board) {
    await notify(
      "No board yet",
      "This tenant has no board to file a ticket on. Create one on the Board page first.",
    );
    return null;
  }

  const { data, error } = await api.POST("/api/v1/boards/{id}/tasks", {
    params: { path: { id: board.id } },
    body: {
      title: PORTS_TICKET_TITLE,
      description: portsTicketBody(workspaceName),
      workspace_id: workspaceId,
      // Triage, by TYPE not by id — the backlog column can be renamed and the
      // type survives it. A human promotes it and applies `agent-ready`; nothing
      // here does either.
      column_type: "backlog",
    },
  });
  if (error || !data?.key) {
    await notify("Could not file the ticket", "The control plane rejected it.");
    return null;
  }
  return { key: data.key, existed: false };
}
