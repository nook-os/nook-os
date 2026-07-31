// MAIN-323: the two grouping rules that a screenshot cannot check.
import { describe, expect, it } from "vitest";
import { ADHOC_GROUP, groupTabs, hueOf, visibleTabs } from "./tabGroups";
import type { SessionTab } from "./sessionTabsStore";

const tab = (id: string, ws?: string, wsName?: string, node?: string): SessionTab => ({
  id,
  name: id,
  runtime: "bash",
  workspaceId: ws,
  workspaceName: wsName,
  nodeName: node,
});

describe("groupTabs", () => {
  it("groups by workspace and keeps tab order inside a group", () => {
    const tabs = [tab("a", "w1", "api"), tab("b", "w2", "web"), tab("c", "w1", "api")];
    const groups = groupTabs(tabs, []);
    expect(groups.map((g) => g.key)).toEqual(["w1", "w2"]);
    expect(groups[0].tabs.map((t) => t.id)).toEqual(["a", "c"]);
    expect(groups[0].label).toBe("api");
  });

  it("orders groups by first appearance, so a pin still decides what is leftmost", () => {
    // Sorting groups by name would silently override the user's pin/drag order,
    // which the strip has already applied by the time it gets here.
    const tabs = [tab("z", "w2", "zebra"), tab("a", "w1", "apple")];
    expect(groupTabs(tabs, []).map((g) => g.label)).toEqual(["zebra", "apple"]);
  });

  it("puts workspace-less terminals in their own real group", () => {
    // A real key, not `undefined`, so it can be collapsed and remembered like
    // any other group.
    const groups = groupTabs([tab("t")], []);
    expect(groups[0].key).toBe(ADHOC_GROUP);
    expect(groups[0].label).toBe("Terminals");
  });

  it("collapses the groups it is told to", () => {
    const tabs = [tab("a", "w1", "api"), tab("b", "w2", "web")];
    const groups = groupTabs(tabs, ["w1"]);
    expect(groups[0].collapsed).toBe(true);
    expect(groups[1].collapsed).toBe(false);
    // Collapsing hides the tabs but never removes them from the group.
    expect(groups[0].tabs).toHaveLength(1);
  });

  it("expands a collapsed group that holds the ACTIVE session", () => {
    // The strip is how you know where you are. Hiding the tab you are looking
    // at leaves the terminal below it unexplained.
    const tabs = [tab("a", "w1", "api"), tab("b", "w2", "web")];
    const groups = groupTabs(tabs, ["w1", "w2"], "a");
    expect(groups[0].collapsed).toBe(false);
    expect(groups[1].collapsed).toBe(true);
  });

  it("gives one workspace one stable colour, and different ones different colours", () => {
    expect(hueOf("w1")).toBe(hueOf("w1"));
    expect(hueOf("w1")).not.toBe(hueOf("w2"));
    // Workspace ids are sequential UUIDv7s, so adjacent ids must not come out
    // as neighbouring colours.
    const a = hueOf("019f840f-2d80-7163-b4b1-8b1e12d7e0d3");
    const b = hueOf("019f840f-2d80-7163-b4b1-8b1e12d7e0d4");
    expect(Math.abs(a - b)).toBeGreaterThan(10);
  });
});

describe("visibleTabs", () => {
  it("omits the tabs of collapsed groups", () => {
    // Keyboard switching walks this list. Landing on a hidden tab would move
    // the terminal to a session the strip is not showing.
    const tabs = [tab("a", "w1", "api"), tab("b", "w2", "web"), tab("c", "w1", "api")];
    expect(visibleTabs(groupTabs(tabs, ["w1"])).map((t) => t.id)).toEqual(["b"]);
  });

  it("keeps strip order across groups", () => {
    const tabs = [tab("a", "w1"), tab("b", "w2"), tab("c", "w1")];
    expect(visibleTabs(groupTabs(tabs, [])).map((t) => t.id)).toEqual(["a", "c", "b"]);
  });
});
