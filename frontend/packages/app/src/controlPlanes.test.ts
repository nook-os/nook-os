import { beforeEach, describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";

// `setActiveControlPlane` is the desktop bridge the switch calls; hoisted so the
// mock factory (hoisted above imports by vitest) can reference it.
const { setActive, localState, probe } = vi.hoisted(() => ({
  setActive: vi.fn(async () => {}),
  // What the shell last reported about the bundled stack; the tests move it.
  localState: { current: null as { base_url: string; ready: boolean } | null },
  probe: vi.fn(async () => ({ ok: true, detail: "" })),
}));
vi.mock("./desktop", () => ({
  isDesktop: () => false,
  LOCAL_CONTROL_PLANE: "local",
  isLocalPlane: (cp: { base_url: string; kind?: string }) =>
    cp.kind === "local" || cp.base_url === "local",
  knownLocalStack: () => localState.current,
  setActiveControlPlane: setActive,
  forgetControlPlane: vi.fn(async () => {}),
  renameControlPlane: vi.fn(async () => {}),
  listControlPlanes: vi.fn(async () => ({ control_planes: [], active: null })),
  probeControlPlane: probe,
}));

import {
  displayName,
  healthCache,
  hostOf,
  probeCached,
  probeTarget,
  subtitleOf,
  switchToControlPlane,
} from "./controlPlanes";

const local = { base_url: "local", token: "", kind: "local" as const };
const remote = { base_url: "https://a.example.com", token: "t" };

beforeEach(() => {
  setActive.mockClear();
  probe.mockClear();
  healthCache.clear();
  localState.current = null;
});

describe("hostOf", () => {
  it("extracts the host, and degrades gracefully on a non-URL", () => {
    expect(hostOf("https://nook.example.com:8443/board")).toBe(
      "nook.example.com:8443",
    );
    expect(hostOf("http://localhost:8080")).toBe("localhost:8080");
    expect(hostOf("garbage")).toBe("garbage");
  });
});

// AC-4: the row reads as local. There is no host on it, and nothing on it is
// presented as a URL — not the name, not the subtitle, not the tooltip.
describe("the Local row reads as local", () => {
  it("is named Local and says where it runs instead of naming a host", () => {
    expect(displayName(local)).toBe("Local");
    expect(subtitleOf(local)).toBe("runs on this computer");
    // Never the address it happens to be listening on this launch.
    localState.current = { base_url: "http://127.0.0.1:41007", ready: true };
    expect(subtitleOf(local)).not.toContain("127.0.0.1");
    expect(displayName(local)).not.toContain("127.0.0.1");
  });

  it("still shows a remote's host, unchanged (NG-2)", () => {
    expect(displayName(remote)).toBe("a.example.com");
    expect(subtitleOf(remote)).toBe("a.example.com");
    expect(displayName({ ...remote, label: "work" })).toBe("work");
  });
});

// The key a row is filed under is not always where it can be reached: Local's
// address is whichever port the bundled stack took this launch.
describe("probeTarget", () => {
  it("resolves Local to the running stack, and a remote to itself", () => {
    localState.current = { base_url: "http://127.0.0.1:41007", ready: true };
    expect(probeTarget(local)).toBe("http://127.0.0.1:41007");
    expect(probeTarget(remote)).toBe(remote.base_url);
  });

  it("is empty when no local stack has answered", () => {
    expect(probeTarget(local)).toBe("");
  });

  it("probes the ADDRESS but caches under the KEY", async () => {
    localState.current = { base_url: "http://127.0.0.1:41007", ready: true };
    await probeCached("local", probeTarget(local));
    expect(probe).toHaveBeenCalledWith("http://127.0.0.1:41007");
    // Cached under the stable key, so the next launch's port cannot inherit a
    // verdict about this one.
    expect(healthCache.get("local")?.ok).toBe(true);
  });

  it("reports a stack with no address as down without fetching", async () => {
    expect(await probeCached("local", "")).toBe(false);
    expect(probe).not.toHaveBeenCalled();
  });
});

describe("switchToControlPlane", () => {
  it("is a no-op on the already-active server (no set-active, no reload)", async () => {
    const reload = vi.fn();
    const switched = await switchToControlPlane("https://a", "https://a", reload);
    expect(switched).toBe(false);
    expect(setActive).not.toHaveBeenCalled();
    expect(reload).not.toHaveBeenCalled();
  });

  it("sets active THEN reloads when switching to a different server", async () => {
    const reload = vi.fn();
    const switched = await switchToControlPlane("https://b", "https://a", reload);
    expect(switched).toBe(true);
    expect(setActive).toHaveBeenCalledWith("https://b");
    expect(reload).toHaveBeenCalledTimes(1);
  });

  // AC-1: Local switches like any other row, by its stable key — the port it
  // answers on this launch never reaches the store.
  it("switches to and from Local by its key", async () => {
    const reload = vi.fn();
    localState.current = { base_url: "http://127.0.0.1:41007", ready: true };

    expect(await switchToControlPlane("local", "https://a", reload)).toBe(true);
    expect(setActive).toHaveBeenLastCalledWith("local");

    expect(await switchToControlPlane("https://a", "local", reload)).toBe(true);
    expect(setActive).toHaveBeenLastCalledWith("https://a");
    expect(setActive).not.toHaveBeenCalledWith("http://127.0.0.1:41007");
  });

  it("is a no-op on Local when Local is already active", async () => {
    const reload = vi.fn();
    expect(await switchToControlPlane("local", "local", reload)).toBe(false);
    expect(setActive).not.toHaveBeenCalled();
    expect(reload).not.toHaveBeenCalled();
  });
});

// AC (test expectations): the switch/health/manage logic is a SINGLE
// implementation both the pill and the tabs consume — guarding against the two
// switchers drifting.
describe("pill and tabs share the one control-plane implementation", () => {
  const read = (f: string) => readFileSync(new URL(f, import.meta.url), "utf8");
  it("both import the shared module and define no duplicate switch/probe logic", () => {
    for (const file of ["./ControlPlanePill.tsx", "./ControlPlaneTabs.tsx"]) {
      const src = read(file);
      expect(src).toContain('from "./controlPlanes"');
      // The health probe and host helper live ONLY in controlPlanes.ts.
      expect(src).not.toMatch(/async function probeCached/);
      expect(src).not.toMatch(/function hostOf/);
      expect(src).not.toMatch(/const healthCache =/);
      // And so does what a row is CALLED — otherwise one surface could keep
      // labelling Local with a host while the other does not (AC-4).
      expect(src).toContain("displayName(cp)");
      expect(src).toContain("subtitleOf(cp)");
    }
  });
});
