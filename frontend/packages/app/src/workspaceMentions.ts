// What `@` completes to, and where a completed `@slug` goes (MAIN-633).
//
// The editor in `@nookos/ui` knows how to open a menu and insert a slug; it
// deliberately does not know which endpoint answers "which workspaces may I
// mention". That join is here, which is also the only place that has to change
// if mentions ever mean something other than a workspace.
import { api, type WorkspaceRef } from "@nookos/api";
import type { MentionLink, MentionOption, MentionSource } from "@nookos/ui";

/** The tenant's workspaces whose slug or name starts with what has been typed.
 *
 *  No debounce: the reply is capped at ten rows of two columns and a mention is
 *  a handful of keystrokes, so a trailing delay would only make the menu appear
 *  late — and the editor already drops an answer that a later keystroke has
 *  overtaken, which is the failure a debounce is usually reached for. */
export const workspaceMentions: MentionSource = {
  async search(query: string): Promise<MentionOption[]> {
    const { data } = await api.GET("/api/v1/workspaces/mentionable", {
      params: { query: { q: query } },
    });
    return data ?? [];
  },
};

/** The card's RESOLVED references, as links (AC-5).
 *
 *  Built from what the server stored, never from re-parsing the body: a slug
 *  the server did not resolve is absent here and therefore renders as plain
 *  text, which is exactly the distinction the reader needs to see. */
export function mentionLinks(refs: WorkspaceRef[] | undefined): MentionLink[] {
  return (refs ?? []).map((r) => ({
    slug: r.slug,
    href: `/workspaces/${r.workspace_id}`,
  }));
}
