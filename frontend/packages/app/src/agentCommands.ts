// The two command endpoints the AGENT surfaces serve (MAIN-530), as one hook.
//
// A chat session and a loop run are the control plane's, and they answer the
// same request and response shapes team chat's channels do — so this is a
// transport, not a feature. It fetches the SERVER's list, posts a name and the
// rest of the line back, and returns what came out. It implements, transforms
// and special-cases no command, which is what `serverOwnedCommands.test.ts`
// reads the tree for (MAIN-529 AC-2).
//
// One hook for all four surfaces rather than four wirings, so a surface cannot
// grow its own idea of what a command is.
import { useCallback } from "react";
import { useQuery } from "@tanstack/react-query";
import { api, READ_ONLY_POST, type ChatCommand, type ChatCommandResult } from "@nookos/api";

/** Which control-plane surface the commands belong to. The two take different
 *  paths and nothing else. */
export type AgentSurface = "session" | "run";

export const agentCommandsKey = (surface: AgentSurface, id: string) =>
  ["agent-commands", surface, id] as const;

export async function fetchAgentCommands(
  surface: AgentSurface,
  id: string,
): Promise<ChatCommand[]> {
  const res =
    surface === "session"
      ? await api.GET("/api/v1/sessions/{id}/commands", { params: { path: { id } } })
      : await api.GET("/api/v1/jobs/{id}/commands", { params: { path: { id } } });
  return res.data ?? [];
}

/** What a refusal SAID, for the note the composer renders where the command was
 *  typed. The server's sentence names the command and where to look; a status
 *  line does neither. */
function refusalText(error: unknown): string {
  if (error && typeof error === "object") {
    const body = error as { error?: string; message?: string };
    if (body.error) return body.error;
    if (body.message) return body.message;
  }
  return "that command could not be run";
}

/**
 * Run a command as the caller.
 *
 * `READ_ONLY_POST` for the reason chat's own client gives: a refusal here is
 * the server ANSWERING what somebody typed, not a write that silently vanished,
 * so it must not raise the global write-failure toast. It is thrown instead,
 * and `ChatView` renders it inline where the command was typed.
 */
export async function runAgentCommand(
  surface: AgentSurface,
  id: string,
  name: string,
  args: string,
): Promise<ChatCommandResult> {
  const body = { name, args };
  const res =
    surface === "session"
      ? await api.POST("/api/v1/sessions/{id}/commands", {
          params: { path: { id } },
          headers: READ_ONLY_POST,
          body,
        })
      : await api.POST("/api/v1/jobs/{id}/commands", {
          params: { path: { id } },
          headers: READ_ONLY_POST,
          body,
        });
  if (res.error) throw new Error(refusalText(res.error));
  return res.data ?? {};
}

/**
 * The commands this surface offers, and the one call that runs them — exactly
 * the pair `ChatView` takes.
 *
 * `id` may be absent (no run yet, no session selected): the query simply does
 * not run and the composer gets no palette, which is the same state every
 * surface had before this existed.
 */
export function useAgentCommands(surface: AgentSurface, id: string | null | undefined) {
  const { data } = useQuery({
    queryKey: agentCommandsKey(surface, id ?? "none"),
    enabled: !!id,
    queryFn: () => fetchAgentCommands(surface, id!),
    // The set changes when the server ships a new one, not while a page is
    // open — so it is worth no refetch of its own.
    staleTime: Infinity,
  });
  const onCommand = useCallback(
    (name: string, args: string) => runAgentCommand(surface, id!, name, args),
    [surface, id],
  );
  return { commands: data, onCommand };
}
