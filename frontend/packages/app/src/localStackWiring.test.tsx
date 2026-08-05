// MAIN-396 AC-3, the half a loop review found missing on PR #323: the shell
// captured the failed control plane's log, `desktop.ts` exposed it — and no
// component called either, so an unwritable database file still produced the
// blank window the card exists to prevent.
//
// Two assertions, because the defect had two halves and either alone would let
// it come back:
//
//   1. the boot path CALLS `awaitLocalStack` (a render test cannot see this —
//      the component renders fine while nothing feeds it)
//   2. the failure SURFACE renders the child's log verbatim
import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { LocalStackFailed } from "./index";

describe("the bundled control plane's failure reaches the UI", () => {
  it("is awaited by the boot path, not merely exported", () => {
    // Source-level on purpose. The regression was an unused export, which
    // typechecks, renders and tests green everywhere — the only thing that
    // distinguishes it is whether the boot path names it.
    // Resolved from the runner's cwd rather than `import.meta.url`: vitest
    // rewrites the latter to a non-file scheme, and this file only needs to
    // find its own sibling.
    const src = readFileSync(resolve(process.cwd(), "src/index.tsx"), "utf8");
    expect(src).toContain("awaitLocalStack");
    expect(
      /await\s+awaitLocalStack\(/.test(src),
      "index.tsx must AWAIT the shell's boot result, not just import it",
    ).toBe(true);
    expect(
      /setLocalStackError\(/.test(src),
      "the awaited result must be stored, or nothing can render it",
    ).toBe(true);
  });

  it("renders the log verbatim rather than a blank window", () => {
    const log = "migration failed: attempt to write a readonly database";
    render(<LocalStackFailed log={log} />);
    expect(screen.getByTestId("local-stack-log").textContent).toBe(log);
    expect(screen.getByText(/did not start/i)).toBeTruthy();
  });
});
