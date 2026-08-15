// Which runtimes the fleet can actually launch (MAIN-600 AC-3).
//
// One list, computed once: the session-policy editor and the New Work picker
// both offer "a runtime", and two copies of the union are two answers to the
// same question — the pair drifts the moment either grows a rule about what
// counts.
import { useQuery } from "@tanstack/react-query";
import { api, type NodeInfo } from "@nookos/api";

/** What a node reports it can run. `capabilities` is opaque on the wire, so
 *  the read is narrowed here rather than at each call site. */
function reported(node: NodeInfo): string[] {
  return (
    ((node.capabilities as Record<string, unknown>)?.runtimes as string[] | undefined) ?? []
  );
}

/**
 * The UNION across the fleet, not one node's — a declaration asking for
 * `claude` lands wherever claude exists, and the picker offering it is what
 * lets somebody say so.
 *
 * Falls back to `["bash"]` when nothing reports anything: an empty picker is
 * unusable, and every machine that can run a session can run a shell.
 */
export function fleetRuntimes(nodes: NodeInfo[] | undefined): string[] {
  const set = new Set<string>();
  for (const n of nodes ?? []) for (const r of reported(n)) set.add(r);
  return set.size ? [...set] : ["bash"];
}

/** The same union, read from the shared `["nodes"]` cache so a surface that
 *  wants the list does not fetch a second copy of the node table. */
export function useFleetRuntimes(): string[] {
  const { data: nodes } = useQuery({
    queryKey: ["nodes"],
    queryFn: async () => (await api.GET("/api/v1/nodes")).data ?? [],
  });
  return fleetRuntimes(nodes);
}
