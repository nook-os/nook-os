import { describe, expect, it } from "vitest";
import type { Overview, OverviewCheckout } from "@nookos/api";
import {
  canAddWorktree,
  canOpenTerminal,
  filterOverview,
  groupCheckoutsByNode,
  isMissing,
  matchesQuery,
} from "./mission";

const co = (over: Partial<OverviewCheckout>): OverviewCheckout => ({
  id: "c1",
  node_id: "n1",
  node_name: "alpha",
  node_status: "online",
  path: "/srv/repo",
  branch: "main",
  kind: "clone",
  dirty: false,
  missing_at: null,
  sessions: [],
  ...over,
});

describe("mission derivations (MAIN-226)", () => {
  it("groups checkouts by node, preserving order", () => {
    const groups = groupCheckoutsByNode([
      co({ id: "a", node_id: "n1", node_name: "alpha" }),
      co({ id: "b", node_id: "n2", node_name: "beta" }),
      co({ id: "c", node_id: "n1", node_name: "alpha" }),
    ]);
    expect(groups.map((g) => g.nodeId)).toEqual(["n1", "n2"]);
    expect(groups[0].checkouts.map((c) => c.id)).toEqual(["a", "c"]);
    expect(groups[1].checkouts.map((c) => c.id)).toEqual(["b"]);
  });

  it("ghosts missing checkouts and gates the actions", () => {
    const clone = co({ kind: "clone" });
    const worktree = co({ kind: "worktree" });
    const missing = co({ kind: "clone", missing_at: "2026-07-29T00:00:00Z" });

    // "+ worktree" is clone-only and never on a missing row.
    expect(canAddWorktree(clone)).toBe(true);
    expect(canAddWorktree(worktree)).toBe(false);
    expect(canAddWorktree(missing)).toBe(false);

    // "terminal here" is any present checkout.
    expect(canOpenTerminal(clone)).toBe(true);
    expect(canOpenTerminal(missing)).toBe(false);

    expect(isMissing(missing)).toBe(true);
    expect(isMissing(clone)).toBe(false);
  });

  it("filters workspaces by repo / node / branch / session text", () => {
    const ov: Overview = {
      workspaces: [
        {
          id: "w1",
          name: "acme-api",
          slug: "acme-api",
          git_remote_url: "git@github.com:acme/api.git",
          git_remote_normalized: "github.com/acme/api",
          checkouts: [
            co({
              id: "c1",
              node_name: "builder-1",
              branch: "feature/login",
              sessions: [
                {
                  id: "s1",
                  name: "claude-work",
                  runtime: "claude",
                  status: "running",
                  created_by: "u1",
                } as never,
              ],
            }),
          ],
          unbound_sessions: [],
        },
        {
          id: "w2",
          name: "web",
          slug: "web",
          git_remote_url: null,
          git_remote_normalized: null,
          checkouts: [co({ id: "c2", node_name: "laptop", branch: "main" })],
          unbound_sessions: [],
        },
      ],
      loose_sessions: [],
    };

    expect(matchesQuery(ov.workspaces[0], "")).toBe(true); // empty keeps all
    expect(filterOverview(ov, "acme").map((w) => w.id)).toEqual(["w1"]); // repo name
    expect(filterOverview(ov, "laptop").map((w) => w.id)).toEqual(["w2"]); // node name
    expect(filterOverview(ov, "feature/login").map((w) => w.id)).toEqual(["w1"]); // branch
    expect(filterOverview(ov, "claude-work").map((w) => w.id)).toEqual(["w1"]); // session name
    expect(filterOverview(ov, "nomatch")).toEqual([]);
    expect(filterOverview(undefined, "x")).toEqual([]);
  });
});
