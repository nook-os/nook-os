// Returning to a section should land where you left it — and never where
// somebody ELSE left it.
import { beforeEach, describe, expect, it } from "vitest";
import { forget, recall, remember } from "./lastPlace";

describe("last place, per tenant", () => {
  beforeEach(() => window.localStorage.clear());

  it("remembers and recalls within one tenant", () => {
    remember("t1", "board.view", "backlog");
    expect(recall("t1", "board.view")).toBe("backlog");
  });

  it("NEVER leaks across tenants", () => {
    // The reason the key is scoped at all. A session id from one tenant is
    // meaningless in another; restoring it would 404 at best, and open the
    // wrong thing at worst.
    remember("t1", "session.id", "sess-from-tenant-one");
    expect(recall("t2", "session.id")).toBeNull();
  });

  it("has no memory rather than a shared one when the tenant is unknown", () => {
    // Before `/auth/me` resolves there is no tenant. Falling back to a shared
    // key would put one tenant's last place where another could read it.
    remember(undefined, "session.id", "sess-1");
    expect(recall(undefined, "session.id")).toBeNull();
    expect(recall("t1", "session.id")).toBeNull();
    remember("   ", "session.id", "sess-2");
    expect(recall("   ", "session.id")).toBeNull();
  });

  it("keeps different things apart", () => {
    remember("t1", "board.view", "backlog");
    remember("t1", "session.id", "sess-9");
    expect(recall("t1", "board.view")).toBe("backlog");
    expect(recall("t1", "session.id")).toBe("sess-9");
  });

  it("forgets", () => {
    remember("t1", "session.id", "sess-9");
    forget("t1", "session.id");
    expect(recall("t1", "session.id")).toBeNull();
  });

  it("survives storage being unavailable", () => {
    // Private mode, disabled storage. A remembered tab is not worth a crash.
    const real = window.localStorage.getItem;
    Object.defineProperty(window.localStorage, "getItem", {
      configurable: true,
      value: () => {
        throw new Error("storage disabled");
      },
    });
    expect(() => recall("t1", "board.view")).not.toThrow();
    expect(recall("t1", "board.view")).toBeNull();
    Object.defineProperty(window.localStorage, "getItem", {
      configurable: true,
      value: real,
    });
  });
});
