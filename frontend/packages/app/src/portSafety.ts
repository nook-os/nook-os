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
 *  The half that matters is "the app READS it". A declaration the app ignores
 *  leases a port and changes nothing — nook hands it `PORT=41007` and the app
 *  still binds 3000, and the second session still collides. The cap lifts on the
 *  DECLARATION ALONE, so a ticket can close with the bug fully intact.
 *
 *  The previous version said exactly that and then did not close the hole
 *  (MAIN-426). Three ways it could be satisfied while leaving the collision:
 *
 *  1. Its NG-4 offered an empty `[[ports]]` list as an equally good outcome. An
 *     empty declaration LIFTS THE CAP (deliberately — `port_safety` filters only
 *     `null`), so the cheapest way to close the ticket was to claim the repo
 *     binds nothing. If that claim is wrong the cap is gone and every hardcoded
 *     port remains: strictly worse than never filing it. "This repo binds
 *     nothing" and "I did not find where this repo binds" produced the same
 *     artifact.
 *  2. Nothing made the builder SHOW the listeners they found, so partial
 *     coverage was invisible to a reviewer. This repo has eleven; three could be
 *     declared, wired and demonstrated while eight stayed hardcoded.
 *  3. "Both come up and neither reports a port in use" tests BINDING only. It
 *     misses port-DERIVED values — proxy targets, redirect and callback URLs,
 *     generated install commands. `frontend/apps/web/vite.config.ts` documents
 *     that exact bug in this repo and would have passed.
 *
 *  Hence: an inventory (AC-1), evidence for an empty declaration (AC-2), a
 *  second instance EXERCISED rather than started (AC-4), and a guard left behind
 *  so the next change cannot undo it (AC-6). */
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
    "**The cap lifts on the declaration alone.** Nook leases a number and sets the",
    "variable; nothing checks that the app reads it. So this ticket can be closed",
    "with the collision fully intact — and an empty declaration closes it fastest",
    "of all. The criteria below exist to make that impossible rather than to",
    "describe it.",
    "",
    "## Acceptance Criteria",
    "",
    "- [ ] AC-1 — **A bind-site inventory, in this ticket.** Every listener this",
    "      repo opens: where it is (`file:line`), what it binds today, and which",
    "      declared `env` it reads after the change. One row per listener. This is",
    "      the artifact the rest of the criteria are checked against — without it",
    "      partial coverage is invisible, and declaring three of eleven looks",
    "      identical to declaring all of them.",
    "",
    "      | listener | where | before | reads now |",
    "      |---|---|---|---|",
    "      | web | src/server.ts:14 | 3000 | `PORT` |",
    "",
    "- [ ] AC-2 — A `.nook.toml` at the repo root declares one `[[ports]]` entry",
    "      per listener in that inventory, each with a stable `name` and the `env`",
    "      the app reads:",
    "",
    "      ```toml",
    "      [[ports]]",
    '      name = "web"',
    '      env  = "PORT"',
    "      ```",
    "",
    "- [ ] AC-3 — Every listener in the inventory reads its port from its declared",
    "      variable. A port literal is CORRECT as the fallback — `PORT ?? 3000` —",
    "      and wrong as the only source. With the variable unset the app still",
    "      starts on whatever port it used before, because a plain `git clone` must",
    "      not start depending on a lease.",
    "- [ ] AC-4 — **Two instances run at once, and the second is EXERCISED, not",
    "      merely started.** With instance A running, drive one real end-to-end",
    "      path against B — sign in, load a page that fetches, follow a redirect —",
    "      and confirm every request it issues addresses B's port. This covers",
    "      port-DERIVED values, which a start-up check cannot see: proxy targets,",
    "      `x-forwarded-*`, redirect and callback URLs, generated install commands.",
    "      A second instance that boots cleanly and then sends you to the first is",
    "      the exact bug this ticket exists to prevent.",
    "- [ ] AC-5 — **Evidence, pasted here, not asserted.** The request list or",
    "      output showing B's port throughout and A's port absent. \"I checked\" is",
    "      not evidence. **A green test suite is not evidence either** — the suite",
    "      boots one instance and passes whether or not the second one works.",
    "- [ ] AC-6 — **A guard, wired into this repo's own test command**, so the next",
    "      change cannot quietly undo this. It fails in BOTH directions: a declared",
    "      `env` that nothing reads, and a bind or publish site whose port does not",
    "      come from a declared variable. It must NOT flag a fallback literal —",
    "      that is AC-3's correct shape, and a guard that flags correct code gets",
    "      suppressed everywhere and protects nothing.",
    "",
    "      The consumer it checks is whatever mechanically reads the declaration in",
    "      THIS repo: a compose file, the application's own bind site, a Helm",
    "      chart, a Procfile, a systemd unit. Something must read it; which one is",
    "      the repo's business.",
    "- [ ] AC-7 — Anything that documents a fixed port (README, compose file, docs)",
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
    "- NG-4 — **If this repo genuinely binds nothing, say so WITH the search that",
    "  came back empty** — what you grepped for, across which paths, and that it",
    "  returned nothing. An empty `[[ports]]` list is a valid statement and it",
    "  LIFTS THE CAP, so it is not the cheap way out: if the claim is wrong, every",
    "  hardcoded port stays and the protection is gone. That is worse than leaving",
    "  this ticket open.",
    "",
    "  This is not meant to tax the honest case. The evidence for \"nothing\" is a",
    "  search that came back empty, not a proof of absence — one grep and its",
    "  output closes this ticket.",
    "",
    "## Relevant files",
    "",
    "- `.nook.toml` — new, at the repo root",
    "- Wherever the app calls listen/bind, and anything that passes it a port",
    "- Anything that DERIVES a URL from one: proxy config, redirect/callback URLs,",
    "  printed commands (AC-4)",
    "- README / compose / docs that name a fixed port (AC-7)",
    "",
    "## Test expectations",
    "",
    "- The app starts with the variable set and listens on that port.",
    "- The app starts with the variable unset and listens on its previous default",
    "  (AC-3) — a regression here breaks every non-nook checkout.",
    "- The AC-6 guard fails when a declared variable loses its consumer, and when a",
    "  bind site stops reading one. Both directions, or it only half works.",
    "",
    "## How to verify",
    "",
    "1. `PORT=41001 <run the app>` — it listens on 41001.",
    "2. In a second shell, `PORT=41002 <run the app>` — it also comes up. Neither",
    "   reports a port already in use.",
    "3. With both still running, drive a real path against the 41002 instance and",
    "   watch the requests. Every one addresses 41002; none addresses 41001.",
    "   Paste that list (AC-5).",
    "4. `<run the app>` with nothing set — it starts as it always did.",
    "5. Break the wiring on purpose: remove a declared variable from its consumer",
    "   and confirm the AC-6 guard reddens; restore it and confirm it greens.",
    "6. Open two nook sessions on the same node. Both start; the one-per-node cap",
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
