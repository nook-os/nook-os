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
  },
});
