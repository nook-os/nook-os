import { beforeEach, describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";

// `setActiveControlPlane` is the desktop bridge the switch calls; hoisted so the
// mock factory (hoisted above imports by vitest) can reference it.
const { setActive } = vi.hoisted(() => ({ setActive: vi.fn(async () => {}) }));
vi.mock("./desktop", () => ({
  isDesktop: () => false,
  setActiveControlPlane: setActive,
  forgetControlPlane: vi.fn(async () => {}),
  renameControlPlane: vi.fn(async () => {}),
  listControlPlanes: vi.fn(async () => ({ control_planes: [], active: null })),
  probeControlPlane: vi.fn(async () => ({ ok: true, detail: "" })),
}));

import { hostOf, switchToControlPlane } from "./controlPlanes";

beforeEach(() => setActive.mockClear());

describe("hostOf", () => {
  it("extracts the host, and degrades gracefully on a non-URL", () => {
    expect(hostOf("https://nook.example.com:8443/board")).toBe(
      "nook.example.com:8443",
    );
    expect(hostOf("http://localhost:8080")).toBe("localhost:8080");
    expect(hostOf("garbage")).toBe("garbage");
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
    }
  });
});
