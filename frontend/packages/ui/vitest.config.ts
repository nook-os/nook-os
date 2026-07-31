import { defineConfig } from "vitest/config";

// jsdom rather than node because the components here touch the DOM. The editing
// transforms MAIN-16 must keep byte-identical are exported as pure functions and
// asserted directly on document strings: CodeMirror's DOM measurement is
// unreliable under jsdom, so driving a full view through it would be flaky,
// whereas the transform logic is exactly what a document-string assertion pins.
export default defineConfig({
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.ts"],
    // Mock hygiene, on for every file (MAIN-303): a `mockImplementation` or
    // `vi.spyOn` set in one test cannot outlive its `it`. The full account of
    // the leak this prevents, and why restoring (rather than blanking) an
    // implementation makes it safe to turn on globally, is in
    // `packages/app/vitest.config.ts` — kept in one place so the two copies
    // cannot drift. `src/mockIsolation.test.ts` is what proves it is still on.
    mockReset: true,
    restoreMocks: true,
  },
});
