import { describe, expect, it } from "vitest";
import type { Overview, OverviewCheckout, Session } from "@nookos/api";
import {
  canAddWorktree,
  canOpenTerminal,
  deckStats,
  exceptionCounts,
  groupCheckoutsByNode,
  isMissing,
  lampMatches,
  matchesQuery,
  overlayLive,
  repoRollup,
  visibleRepos,
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

const sess = (id: string, over: Partial<Session> = {}): Session =>
  ({
    id,
    name: `s-${id}`,
    runtime: "bash",
    status: "running",
    created_by: "u1",
    ...over,
  }) as Session;

const ws = (
  id: string,
  checkouts: OverviewCheckout[],
  unbound: Session[] = [],
): Overview["workspaces"][number] => ({
  id,
  name: id,
  slug: id,
  git_remote_url: null,
  git_remote_normalized: null,
  checkouts,
  unbound_sessions: unbound,
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

  it("gates the actions: worktree on present clones, terminal on present rows", () => {
    const clone = co({ kind: "clone" });
    const worktree = co({ kind: "worktree" });
    const missing = co({ kind: "clone", missing_at: "2026-07-29T00:00:00Z" });

    expect(canAddWorktree(clone)).toBe(true);
    expect(canAddWorktree(worktree)).toBe(false);
    expect(canAddWorktree(missing)).toBe(false);

    expect(canOpenTerminal(clone)).toBe(true);
    expect(canOpenTerminal(missing)).toBe(false);

    expect(isMissing(missing)).toBe(true);
    expect(isMissing(clone)).toBe(false);
  });

  it("filters workspaces by repo / node / branch / session text", () => {
    const w = {
      ...ws("acme-api", [
        co({
          id: "c1",
          node_name: "builder-1",
          branch: "feature/login",
          sessions: [sess("s1", { name: "claude-work", runtime: "claude" })],
        }),
      ]),
      git_remote_url: "git@github.com:acme/api.git",
      git_remote_normalized: "github.com/acme/api",
    };
    expect(matchesQuery(w, "")).toBe(true);
    expect(matchesQuery(w, "acme")).toBe(true);
    expect(matchesQuery(w, "builder-1")).toBe(true);
    expect(matchesQuery(w, "feature/login")).toBe(true);
    expect(matchesQuery(w, "claude-work")).toBe(true);
    expect(matchesQuery(w, "nomatch")).toBe(false);
  });
});

describe("the annunciator deck", () => {
  const overview: Overview = {
    workspaces: [
      ws("api", [
        co({ id: "c1", node_id: "n1", sessions: [sess("s1")] }),
        co({ id: "c2", node_id: "n1", kind: "worktree", dirty: true }),
        co({ id: "c3", node_id: "n2", node_status: "offline" }),
      ]),
      ws("web", [
        co({ id: "c4", node_id: "n1", missing_at: "2026-07-29T00:00:00Z" }),
      ]),
    ],
    loose_sessions: [sess("s9")],
  };

  it("counts fleet stats across the whole payload", () => {
    const s = deckStats(overview);
    expect(s).toEqual({
      nodesOnline: 1,
      nodesTotal: 2,
      repos: 2,
      checkouts: 4,
      sessions: 2,
    });
    expect(deckStats(undefined).repos).toBe(0);
  });

  it("counts exceptions: dirty, missing, offline — and nothing else", () => {
    expect(exceptionCounts(overview)).toEqual({
      dirty: 1,
      missing: 1,
      offline: 1,
    });
  });

  it("lampMatches classifies checkouts per lamp", () => {
    const dirty = co({ dirty: true });
    const gone = co({ missing_at: "2026-07-29T00:00:00Z", dirty: true });
    const off = co({ node_status: "offline" });
    expect(lampMatches(dirty, "dirty")).toBe(true);
    expect(lampMatches(gone, "dirty")).toBe(false); // missing outranks dirty
    expect(lampMatches(gone, "missing")).toBe(true);
    expect(lampMatches(off, "offline")).toBe(true);
    expect(lampMatches(dirty, "offline")).toBe(false);
  });
});

describe("visibility: filter → lamp → ghosts", () => {
  const overview: Overview = {
    workspaces: [
      ws("api", [
        co({ id: "live1", sessions: [sess("s1")] }),
        co({ id: "gone1", missing_at: "2026-07-29T00:00:00Z" }),
      ]),
      ws("web", [co({ id: "dirty1", dirty: true })]),
    ],
    loose_sessions: [],
  };

  it("hides ghosts by default and reports the hidden count", () => {
    const repos = visibleRepos(overview, "", null, false);
    const api = repos.find((r) => r.workspace.id === "api")!;
    expect(api.checkouts.map((c) => c.id)).toEqual(["live1"]);
    expect(api.hiddenGhosts).toBe(1);
  });

  it("shows ghosts when the toggle is on", () => {
    const repos = visibleRepos(overview, "", null, true);
    const api = repos.find((r) => r.workspace.id === "api")!;
    expect(api.checkouts.map((c) => c.id)).toEqual(["live1", "gone1"]);
    expect(api.hiddenGhosts).toBe(0);
  });

  it("a lamp narrows the tree to matching rows only", () => {
    const dirty = visibleRepos(overview, "", "dirty", false);
    expect(dirty.map((r) => r.workspace.id)).toEqual(["web"]);
    expect(dirty[0].checkouts.map((c) => c.id)).toEqual(["dirty1"]);

    // The missing lamp overrides the ghosts toggle — it IS a request for ghosts.
    const missing = visibleRepos(overview, "", "missing", false);
    expect(missing.map((r) => r.workspace.id)).toEqual(["api"]);
    expect(missing[0].checkouts.map((c) => c.id)).toEqual(["gone1"]);
  });

  it("free-text filter composes with the rest", () => {
    expect(
      visibleRepos(overview, "web", null, false).map((r) => r.workspace.id),
    ).toEqual(["web"]);
    expect(visibleRepos(overview, "nomatch", null, false)).toEqual([]);
    expect(visibleRepos(undefined, "", null, false)).toEqual([]);
  });
});

describe("rollups and live overlay", () => {
  it("repoRollup counts sessions, checkouts and exceptions", () => {
    const w = ws(
      "api",
      [
        co({ id: "a", sessions: [sess("s1"), sess("s2")] }),
        co({ id: "b", dirty: true }),
        co({ id: "c", missing_at: "2026-07-29T00:00:00Z" }),
      ],
      [sess("s3")],
    );
    expect(repoRollup(w)).toEqual({
      sessions: 3,
      checkouts: 3,
      dirty: 1,
      missing: 1,
    });
  });

  it("overlayLive applies node/session deltas and drops dead sessions", () => {
    const overview: Overview = {
      workspaces: [
        ws("api", [
          co({ id: "c1", node_id: "n1", sessions: [sess("s1"), sess("s2")] }),
        ]),
      ],
      loose_sessions: [sess("s3")],
    };
    const out = overlayLive(
      overview,
      { n1: "offline" },
      { s1: "detached", s2: "exited", s3: "error" },
    )!;
    const c = out.workspaces[0].checkouts[0];
    expect(c.node_status).toBe("offline");
    expect(c.sessions.map((s) => s.id)).toEqual(["s1"]); // s2 died → dropped
    expect(c.sessions[0].status).toBe("detached");
    expect(out.loose_sessions).toEqual([]); // s3 died → dropped
  });
});
