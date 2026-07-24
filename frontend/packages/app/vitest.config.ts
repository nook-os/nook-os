import { defineConfig } from "vitest/config";

// jsdom, not node: what these tests need to check is a click on an anchor —
// the browser behaviour the desktop app has to intercept. Asserting on a pure
// function alone would leave the part that actually broke untested.
export default defineConfig({
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.ts"],
  },
});
