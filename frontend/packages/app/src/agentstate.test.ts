import { describe, expect, it } from "vitest";
import { AGENT_STATE_STALE_MS, liveAgentMark, type AgentState } from "./live";

const agent: AgentState = { state: "running", window: 0, at: 0 };

describe("agent-state indicator robustness (MAIN-13)", () => {
  it("keeps the client staleness window at least the server TTL (15 min)", () => {
    // The server's AGENT_STATE_TTL is 15 min; the client must not fade a mark
    // sooner, or a reload re-seeds it from the server and it flickers back.
    expect(AGENT_STATE_STALE_MS).toBeGreaterThanOrEqual(15 * 60 * 1000);
  });

  it("suppresses the agent mark for a dead session", () => {
    for (const status of ["exited", "error", "killed"]) {
      expect(liveAgentMark(status, agent)).toBeUndefined();
    }
  });

  it("keeps the agent mark for a live or unknown-status session", () => {
    expect(liveAgentMark("running", agent)).toBe(agent);
    expect(liveAgentMark("starting", agent)).toBe(agent);
    expect(liveAgentMark(undefined, agent)).toBe(agent);
  });

  it("is undefined when there is no agent, dead or not", () => {
    expect(liveAgentMark("running", undefined)).toBeUndefined();
    expect(liveAgentMark("killed", undefined)).toBeUndefined();
  });
});
