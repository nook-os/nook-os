// The guard on the guard, for THIS package's config (MAIN-303 AC-3).
//
// `packages/ui` has its own `vitest.config.ts`, so the app package's proof says
// nothing about this one — the setting could be dropped here and every suite
// would stay green until something leaked. The twin in
// `packages/app/src/mockIsolation.test.tsx` carries the full reasoning; this is
// the same claim against the config that governs these tests.
//
// Deliberately dependency-free: `packages/ui`'s vitest only picks up `.test.ts`,
// and a spy on a local object exercises `restoreMocks` and `mockReset` without
// dragging a component or a module mock in to do it.
import { describe, expect, it, vi } from "vitest";

const DEFAULT = "the original implementation";
const OVERRIDE = "a stub from the previous test";

const load = vi.fn(() => DEFAULT);

const clock = {
  now(): string {
    return "the real method";
  },
};

describe("ui: mocks do not leak between tests", () => {
  it("a test may override a mock's implementation", () => {
    load.mockImplementation(() => OVERRIDE);
    expect(load()).toBe(OVERRIDE);
  });

  it("…and the NEXT test sees the implementation it was created with", () => {
    // `mockReset` restores the function passed to `vi.fn`, rather than blanking
    // it — so this is DEFAULT, not `undefined`.
    expect(load()).toBe(DEFAULT);
    expect(load.mock.calls).toHaveLength(1);
  });

  it("a test may spy on a real method", () => {
    vi.spyOn(clock, "now").mockReturnValue("stubbed");
    expect(clock.now()).toBe("stubbed");
  });

  it("…and the NEXT test gets the real method back", () => {
    expect(clock.now()).toBe("the real method");
  });
});
