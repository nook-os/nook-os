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

describe("alternate views", () => {
  const repos = (ov: Overview) =>
    // Ghosts shown so grouping sees every row.
    visibleRepos(ov, "", null, true);

  const overview: Overview = {
    workspaces: [
      ws("api", [
        co({ id: "a1", node_id: "n1", node_name: "alpha" }),
        co({ id: "a2", node_id: "n2", node_name: "beta", kind: "worktree" }),
      ]),
      ws("web", [co({ id: "b1", node_id: "n1", node_name: "alpha" })]),
    ],
    loose_sessions: [],
  };

  it("machineGroups regroups the filtered repos node-first", async () => {
    const { machineGroups } = await import("./mission");
    const groups = machineGroups(repos(overview));
    expect(groups.map((g) => g.nodeId)).toEqual(["n1", "n2"]);
    expect(groups[0].entries.map((e) => e.checkout.id)).toEqual(["a1", "b1"]);
    expect(groups[0].entries.map((e) => e.workspace.id)).toEqual([
      "api",
      "web",
    ]);
    expect(groups[1].entries.map((e) => e.checkout.id)).toEqual(["a2"]);
  });

  it("matrixData lays repos × machines with per-cell checkouts", async () => {
    const { matrixData } = await import("./mission");
    const m = matrixData(repos(overview));
    expect(m.nodes.map((n) => n.id)).toEqual(["n1", "n2"]);
    expect(m.rows.map((r) => r.workspace.id)).toEqual(["api", "web"]);
    expect(m.rows[0].cells["n1"].map((c) => c.id)).toEqual(["a1"]);
    expect(m.rows[0].cells["n2"].map((c) => c.id)).toEqual(["a2"]);
    expect(m.rows[1].cells["n2"]).toBeUndefined(); // web has nothing on beta
  });

  it("view choice persists and falls back to tree on junk", async () => {
    const { loadView, saveView } = await import("./mission");
    window.localStorage.removeItem("nook.mission.view.v1");
    expect(loadView()).toBe("tree");
    saveView("matrix");
    expect(loadView()).toBe("matrix");
    window.localStorage.setItem("nook.mission.view.v1", "nonsense");
    expect(loadView()).toBe("tree");
    window.localStorage.removeItem("nook.mission.view.v1");
  });
});

describe("context bits", () => {
  it("age renders compact relative durations", async () => {
    const { age } = await import("./mission");
    const now = Date.parse("2026-07-29T12:00:00Z");
    expect(age("2026-07-29T11:59:40Z", now)).toBe("now");
    expect(age("2026-07-29T11:55:00Z", now)).toBe("5m");
    expect(age("2026-07-29T10:00:00Z", now)).toBe("2h");
    expect(age("2026-07-26T12:00:00Z", now)).toBe("3d");
    expect(age(undefined, now)).toBe("");
    expect(age("garbage", now)).toBe("");
  });

  it("repoTint is deterministic per slug", async () => {
    const { repoTint } = await import("./mission");
    expect(repoTint("acme-api")).toBe(repoTint("acme-api"));
    expect(repoTint("acme-api")).toMatch(/^hsl\(\d+ 45% 55%\)$/);
    expect(repoTint("acme-api")).not.toBe(repoTint("acme-web"));
  });
});
