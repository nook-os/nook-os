import { defineConfig } from "vitest/config";

// jsdom, not node: what these tests need to check is a click on an anchor —
// the browser behaviour the desktop app has to intercept. Asserting on a pure
// function alone would leave the part that actually broke untested.
export default defineConfig({
  test: {
    environment: "jsdom",
    // `.tsx` too: the assembly test (Operator.test.tsx) renders components, so
    // it needs JSX. Additive — the existing `.test.ts` suites still match.
    include: ["src/**/*.test.{ts,tsx}"],
    // The gaps in jsdom that would otherwise read as component bugs.
    setupFiles: ["src/testSetup.ts"],
    // Mock hygiene, on for every file (MAIN-303).
    //
    // A test set `api.GET.mockImplementation(...)` on the `vi.mock("@nookos/api")`
    // module mock and nothing put it back, so the stub bled into every later
    // test in the file. It surfaced as a false failure in UNRELATED not-found
    // tests — the first ones to actually depend on the fixture the leak had
    // replaced — while the page under test was fine. That is the worst shape a
    // test bug takes: it accuses the wrong code.
    //
    // `mockReset` puts every mock's implementation back to the one it was
    // created with before each test, so a per-test `mockImplementation` override
    // cannot outlive its `it`. Module mocks built as `vi.fn(async () => …)` keep
    // that factory implementation — Vitest 3 changed `mockReset` from "blank the
    // implementation" to "restore the original", which is what makes this safe
    // to turn on globally rather than a rewrite of every mock.
    //
    // `restoreMocks` does the same job for `vi.spyOn`: the real method goes back
    // on the object, so a spy cannot silently stay installed for the rest of the
    // run.
    //
    // Neither setting is exercised by anything that would notice its removal, so
    // `src/mockIsolation.test.ts{,x}` asserts them directly: delete these two
    // lines and those tests go red instead of the suite quietly losing its
    // isolation.
    mockReset: true,
    restoreMocks: true,
  },
});
