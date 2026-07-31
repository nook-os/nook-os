// "New spec" from a workspace (MAIN-298).
//
// Speccing a card used to require a task id: `/loop/:taskId` is the only door
// into the Loop workspace, and a PM standing in a repo has no id to type.
// Pointing the route at a workspace id does not work either — that route wants a
// *task*, so it renders "doesn't exist". The entry point therefore has to make
// the missing piece itself: one click files a draft ticket in this workspace's
// backlog and hands back the identifier to navigate to.
//
// The draft is deliberately bare. It is the ANCHOR a spec run hangs off, not the
// ticket the run produces — `/nook-spec` files that one, and the Loop page links
// to it from the transcript once it does. So there is no title prompt (a form is
// exactly what the one click exists to avoid) and no description: the idea gets
// typed into the seed box on the page this lands you on.
import { api, type Board } from "@nookos/api";
import { notify } from "./dialogs";

/** What a spec anchor is called until a run fleshes it out. */
export const SPEC_DRAFT_TITLE = "New spec";

/**
 * The board this workspace's cards belong on.
 *
 * A board may be bound to one workspace (`boards.workspace_id`), or be the
 * tenant's single shared board — which is the common shape, and what the Board
 * page assumes when it just takes `boards[0]`. So: prefer a board that names
 * this workspace, otherwise fall back to that same first board rather than
 * refusing. A tenant with one board is not a misconfiguration.
 */
export function boardForWorkspace(
  boards: Board[] | undefined,
  workspaceId: string,
): Board | undefined {
  const all = boards ?? [];
  return all.find((b) => b.workspace_id === workspaceId) ?? all[0];
}

/**
 * File the draft ticket and return the route param for its Loop page.
 *
 * `null` means it was not created and the caller has already been told why — so
 * a caller only has to decide whether to navigate.
 */
export async function createSpecDraft(workspaceId: string): Promise<string | null> {
  const boards = (await api.GET("/api/v1/boards")).data ?? [];
  const board = boardForWorkspace(boards, workspaceId);
  if (!board) {
    await notify(
      "No board yet",
      "This tenant has no board to file a ticket on. Create one on the Board page first.",
    );
    return null;
  }

  const { data, error, response } = await api.POST("/api/v1/boards/{id}/tasks", {
    params: { path: { id: board.id } },
    // `column_type`, not a column id: the backlog is "Triage" today and could be
    // renamed tomorrow, and the type is the thing that survives that. A human
    // still promotes the card — nothing here applies `agent-ready`.
    body: {
      title: SPEC_DRAFT_TITLE,
      workspace_id: workspaceId,
      column_type: "backlog",
    },
  });
  if (error || !response?.ok || !data) {
    await notify(
      "Could not start a spec",
      error
        ? String((error as { error?: unknown }).error ?? JSON.stringify(error))
        : (response?.statusText ?? "The ticket could not be created."),
    );
    return null;
  }

  // Prefer the key: it is what a person reads in the address bar and what the
  // board's own menu links with. The route resolves either (MAIN-209).
  return data.key ?? data.id;
}
