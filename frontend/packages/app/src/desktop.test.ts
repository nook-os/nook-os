import { describe, expect, it } from "vitest";
import { sessionTabPrefsKey, sessionTabsKey } from "./desktop";

// AC-8: session tabs are namespaced per control plane. The key derivation is
// the pure core of that — two servers must land on distinct storage keys, so a
// switch swaps the tab strip wholesale instead of showing the other server's
// dead session IDs; the web build keeps the original un-namespaced key.
describe("sessionTabsKey — per-control-plane tab namespacing", () => {
  it("gives two servers distinct, disjoint keys", () => {
    const a = sessionTabsKey("https://a.example.com");
    const b = sessionTabsKey("https://b.example.com");
    expect(a).not.toEqual(b);
    // Distinct keys mean writes under one never appear under the other.
    expect(a.includes("a.example.com")).toBe(true);
    expect(b.includes("a.example.com")).toBe(false);
  });

  it("keeps the original key for the web build (empty active server)", () => {
    expect(sessionTabsKey("")).toBe("nook.session-tabs");
  });

  it("namespaces a desktop server under the shared prefix", () => {
    expect(sessionTabsKey("https://nook.example.com")).toBe(
      "nook.session-tabs::https://nook.example.com",
    );
  });
});

describe("sessionTabPrefsKey — MAIN-322 view prefs", () => {
  it("is namespaced per control plane, like the tab set it replaces", () => {
    expect(sessionTabPrefsKey("https://a.example.com")).not.toBe(
      sessionTabPrefsKey("https://b.example.com"),
    );
  });

  it("never collides with the retired open-set key", () => {
    // They are cleared together on forget, and the prefs loader must not read
    // an old array of session ids as a prefs object.
    for (const cp of ["", "https://nook.example.com"]) {
      expect(sessionTabPrefsKey(cp)).not.toBe(sessionTabsKey(cp));
    }
  });
});
