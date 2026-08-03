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

/** Whether a workspace has declared its listeners, from the workspace ROW.
 *
 *  The list view's half of the cap. `port_capped` comes from
 *  `/workspaces/{id}/reconcile-status`, which is one call per workspace — fine
 *  on a detail page, an N+1 on a table. The declaration itself already rides on
 *  every `Workspace`, and the server derives the cap from exactly this, so the
 *  table can answer without asking.
 *
 *  `null` and absent are undeclared. An EMPTY array is not: it is the workspace
 *  saying "this repo binds nothing", which is a real declaration and lifts the
 *  cap. Collapsing the two is the easy mistake here — it would nag every repo
 *  that has honestly answered. */
export function hasPortDeclaration(w: {
  port_requirements?: unknown;
}): boolean {
  return w.port_requirements !== null && w.port_requirements !== undefined;
}

/** Whether the reconciler is holding this workspace to one session per node.
 *
 *  Derived from what the server derived — never stored on either side, so a
 *  declaration landing lifts it on the next read with nothing to invalidate. */
export function isPortCapped(status: { port_capped?: boolean } | null | undefined): boolean {
  return !!status?.port_capped;
}

/** The ticket body, in the shape `nook-build` actually consumes.
 *
 *  Deliberately a BUILD contract, not a research note. The first version read
 *  as instructions to a human — no acceptance criteria, no non-goals, nothing
 *  to verify — so a builder picking it up had nothing to satisfy and no PR to
 *  open against. The loop implements `AC-N`, treats `NG-N` as binding, and
 *  ships a PR; a ticket without them is a ticket the loop cannot finish.
 *
 *  AC-2 is the one that matters. A declaration the app ignores leases a port and
 *  changes nothing — nook would hand it `PORT=41007` and the app would still
 *  bind 3000, and the second session would still collide. The cap lifts on the
 *  declaration alone, so it is entirely possible to "fix" this ticket and leave
 *  the bug in place; AC-4 is what proves otherwise. */
export function portsTicketBody(workspaceName: string): string {
  return [
    "## Problem",
    "",
    `\`${workspaceName}\` declares no ports, so nook holds it to ONE session per`,
    "node. Two sessions of this repo on one machine would both bind whatever the",
    "app hardcodes, and the second would fail in a way that looks like the app's",
    "fault rather than a collision.",
    "",
    "Lifting the cap takes two things, and the second is the one that is easy to",
    "skip: the repo has to DECLARE its listeners, and the app has to READ them.",
    "",
    "## Acceptance Criteria",
    "",
    "- [ ] AC-1 — A `.nook.toml` at the repo root declares one `[[ports]]` entry",
    "      per listener the app binds, each with a stable `name` and the `env`",
    "      variable the app will read:",
    "",
    "      ```toml",
    "      [[ports]]",
    '      name = "web"',
    '      env  = "PORT"',
    "      ```",
    "",
    "- [ ] AC-2 — Every listener reads its port from that variable. No hardcoded",
    "      port literal remains on any bind path. **This is the half that makes",
    "      the feature work** — nook leases a number and sets the variable; an app",
    "      that ignores it still collides.",
    "- [ ] AC-3 — With the variable unset the app still starts, on whatever port it",
    "      used before. It must keep running outside nook — a plain `git clone`",
    "      and a local dev run cannot start depending on a lease.",
    "- [ ] AC-4 — **Two instances run on one machine at once.** Start the app twice",
    "      with different values for the declared variables; both come up and",
    "      neither reports a port in use. This is the acceptance test — the cap",
    "      lifts on the declaration alone, so without this the ticket can close",
    "      with the collision still there.",
    "- [ ] AC-5 — Anything that documents a fixed port (README, compose file, docs)",
    "      matches the new behaviour, so the next person does not re-hardcode it.",
    "",
    "## Non-goals",
    "",
    "- NG-1 — No reverse proxy, no nice URLs. Leasing a port is this ticket;",
    "  putting a hostname in front of it is separate work.",
    "- NG-2 — No behaviour change beyond WHERE the app listens. Same routes, same",
    "  responses, same everything else.",
    "- NG-3 — The app does not pick or negotiate ports. It reads a variable nook",
    "  set. Any allocation logic added here is a second allocator competing with",
    "  the real one.",
    "- NG-4 — If this repo genuinely binds nothing, do not invent a listener to",
    "  satisfy AC-1. An empty `[[ports]]` list is a valid statement — \"this binds",
    "  nothing\" — and lifts the cap just as well. Say so on the ticket and stop.",
    "",
    "## Relevant files",
    "",
    "- `.nook.toml` — new, at the repo root",
    "- Wherever the app calls listen/bind, and anything that passes it a port",
    "- README / compose / docs that name a fixed port (AC-5)",
    "",
    "## Test expectations",
    "",
    "- The app starts with the variable set and listens on that port.",
    "- The app starts with the variable unset and listens on its previous default",
    "  (AC-3) — a regression here breaks every non-nook checkout.",
    "",
    "## How to verify",
    "",
    "1. `PORT=41001 <run the app>` — it listens on 41001.",
    "2. In a second shell, `PORT=41002 <run the app>` — it also comes up. Neither",
    "   reports a port already in use.",
    "3. `<run the app>` with nothing set — it starts as it always did.",
    "4. Open two nook sessions on the same node. Both start; the one-per-node cap",
    "   is gone from the workspace.",
    "",
    "The cap lifts by itself once the declaration exists — nothing here needs",
    "closing by hand, and closing it without declaring will not lift it.",
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
