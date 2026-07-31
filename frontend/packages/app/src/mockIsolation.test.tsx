// The guard on the guard (MAIN-303 AC-3).
//
// `mockReset` / `restoreMocks` live in `vitest.config.ts`, where nothing
// exercises them: turning either off leaves every suite green until some future
// test leaks a mock into an unrelated one and the failure lands on innocent
// code. That is precisely what happened — `api.GET.mockImplementation(...)` set
// in one `it` bled into the rest of the file, and the red tests were not the
// broken ones.
//
// So this file asserts the SETTING, not any product behaviour. Each pair is two
// sequential `it`s: the first installs a stub the way the original bug did, the
// second asserts the default is back. Delete `mockReset: true` from the config
// and the second `it` of each pair fails — which is the whole point, and is
// checked by hand whenever this file changes (`How to verify`, step 2).
//
// Vitest runs `it`s in declaration order within a file, so "the next test" is
// exactly the one written below.
import { describe, expect, it, vi } from "vitest";

// The same shape as the real leak: a module mock whose members are
// `vi.fn(impl)`. `mockReset` restores that factory implementation (Vitest 3+
// semantics), which is what makes the setting safe to turn on globally instead
// of rewriting every mock in the tree.
const DEFAULT_TITLE = "the default fixture";
const LEAKED_TITLE = "a stub from the previous test";

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async () => ({ data: { title: DEFAULT_TITLE } })),
  },
}));

import { api } from "@nookos/api";

describe("module mocks do not leak between tests (mockReset)", () => {
  it("a test may override the module mock for its own purposes", async () => {
    (api.GET as ReturnType<typeof vi.fn>).mockImplementation(async () => ({
      data: { title: LEAKED_TITLE },
    }));
    const res = (await api.GET("/api/v1/tasks/{id}", {
      params: { path: { id: "t-1" } },
    })) as unknown as { data: { title: string } };
    expect(res.data.title).toBe(LEAKED_TITLE);
  });

  it("…and the NEXT test sees the default fixture, not that override", async () => {
    const res = (await api.GET("/api/v1/tasks/{id}", {
      params: { path: { id: "t-1" } },
    })) as unknown as { data: { title: string } };
    // Without `mockReset: true` this is LEAKED_TITLE, and any test downstream
    // that depended on the real fixture fails while pointing at the wrong code.
    expect(res.data.title).toBe(DEFAULT_TITLE);
  });

  it("call history does not accumulate across tests either", () => {
    // The two `it`s above each called GET once. A reset that only restored
    // implementations while keeping the call log would still let one test
    // assert on another's calls.
    expect((api.GET as ReturnType<typeof vi.fn>).mock.calls).toHaveLength(0);
  });
});

/** A plain object to spy on — `restoreMocks` is about `vi.spyOn`, which
 *  replaces a real method rather than standing in for a whole module. */
const clock = {
  now(): string {
    return "the real method";
  },
};

describe("spies are uninstalled between tests (restoreMocks)", () => {
  it("a test may spy on a real method", () => {
    vi.spyOn(clock, "now").mockReturnValue("stubbed");
    expect(clock.now()).toBe("stubbed");
  });

  it("…and the NEXT test gets the real method back", () => {
    // Without `restoreMocks: true` the spy is still installed here, and every
    // later test in the run silently talks to a stub nobody asked for.
    expect(clock.now()).toBe("the real method");
  });
});
