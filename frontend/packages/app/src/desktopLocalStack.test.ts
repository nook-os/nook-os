// MAIN-396 AC-3: the shell's boot result is what the UI renders instead of a
// blank window, so the polling that fetches it must terminate on BOTH outcomes
// — ready and failed — and must not spin once the shell has answered.
import { describe, expect, it, vi, afterEach } from "vitest";
import { awaitLocalStack } from "./desktop";

function shell(responses: unknown[]) {
  const invoke = vi.fn();
  for (const r of responses) invoke.mockResolvedValueOnce(r);
  (window as unknown as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ = {
    invoke,
  };
  return invoke;
}

afterEach(() => {
  delete (window as unknown as { __TAURI_INTERNALS__?: unknown })
    .__TAURI_INTERNALS__;
});

describe("awaiting the bundled control plane", () => {
  const nosleep = () => Promise.resolve();

  it("stops as soon as it is ready", async () => {
    const invoke = shell([
      { base_url: "", ready: false },
      { base_url: "http://127.0.0.1:41007", ready: true },
    ]);
    const s = await awaitLocalStack(10_000, nosleep);
    expect(s?.ready).toBe(true);
    expect(s?.base_url).toBe("http://127.0.0.1:41007");
    expect(invoke).toHaveBeenCalledTimes(2);
  });

  it("stops on failure and carries the log, rather than polling forever", async () => {
    // The blank-window case. A control plane that dies during migration must
    // surface its own output; spinning until a timeout would show nothing.
    const invoke = shell([
      { base_url: "", ready: false },
      { base_url: "", ready: false, error: "migration failed: disk full" },
    ]);
    const s = await awaitLocalStack(10_000, nosleep);
    expect(s?.ready).toBe(false);
    expect(s?.error).toMatch(/disk full/);
    expect(invoke).toHaveBeenCalledTimes(2);
  });

  it("gives up at the deadline instead of hanging", async () => {
    const invoke = vi.fn().mockResolvedValue({ base_url: "", ready: false });
    (window as unknown as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ =
      { invoke };
    const s = await awaitLocalStack(0, nosleep);
    expect(s?.ready).toBe(false);
    expect(invoke).toHaveBeenCalledTimes(1);
  });

  it("is null off the desktop, which is not an error", async () => {
    expect(await awaitLocalStack(0, nosleep)).toBeNull();
  });
});
