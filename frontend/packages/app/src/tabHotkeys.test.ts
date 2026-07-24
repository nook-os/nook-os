import { afterEach, describe, expect, it, vi } from "vitest";
import React, { act } from "react";
import { createRoot } from "react-dom/client";
import { cycleTab, jumpTab, resolveSwitch, useTabHotkeys } from "./tabHotkeys";

// ── Pure cycle math (no DOM) ────────────────────────────────────────────────

describe("cycleTab", () => {
  const ids = ["a", "b", "c"];

  it("steps to the next tab and wraps at the end", () => {
    expect(cycleTab(ids, "a", 1)).toBe("b");
    expect(cycleTab(ids, "b", 1)).toBe("c");
    expect(cycleTab(ids, "c", 1)).toBe("a"); // wrap last → first
  });

  it("steps to the previous tab and wraps at the start", () => {
    expect(cycleTab(ids, "b", -1)).toBe("a");
    expect(cycleTab(ids, "a", -1)).toBe("c"); // wrap first → last
  });

  it("selects an end tab when nothing is active (the sessions list)", () => {
    expect(cycleTab(ids, undefined, 1)).toBe("a"); // next → first
    expect(cycleTab(ids, undefined, -1)).toBe("c"); // prev → last
  });

  it("is a no-op with zero or one tab in the active set", () => {
    expect(cycleTab([], undefined, 1)).toBeNull();
    expect(cycleTab(["only"], "only", 1)).toBeNull();
    expect(cycleTab(["only"], "only", -1)).toBeNull();
    // One tab but not active yet → stepping in still lands on it.
    expect(cycleTab(["only"], undefined, 1)).toBe("only");
  });
});

describe("jumpTab", () => {
  const ids = ["a", "b", "c", "d"];

  it("jumps to the Nth tab (1-based)", () => {
    expect(jumpTab(ids, 1)).toBe("a");
    expect(jumpTab(ids, 3)).toBe("c");
  });

  it("treats 9 as the last tab regardless of count", () => {
    expect(jumpTab(ids, 9)).toBe("d");
    expect(jumpTab(["x", "y"], 9)).toBe("y");
  });

  it("is a no-op for a position past the tab count", () => {
    expect(jumpTab(ids, 5)).toBeNull();
    expect(jumpTab(ids, 8)).toBeNull();
    expect(jumpTab([], 1)).toBeNull();
  });
});

// ── Classification (pure) ───────────────────────────────────────────────────

const ev = (
  o: Partial<Pick<KeyboardEvent, "key" | "code" | "ctrlKey" | "metaKey" | "altKey" | "shiftKey">>,
) => ({
  key: "",
  code: "",
  ctrlKey: false,
  metaKey: false,
  altKey: false,
  shiftKey: false,
  ...o,
});

describe("resolveSwitch", () => {
  const ids = ["a", "b", "c"];

  it("maps Ctrl+Tab to next and Ctrl+Shift+Tab to previous (both platforms)", () => {
    expect(resolveSwitch(ev({ key: "Tab", ctrlKey: true }), ids, "a", false)).toEqual({
      matched: true,
      id: "b",
    });
    expect(
      resolveSwitch(ev({ key: "Tab", ctrlKey: true, shiftKey: true }), ids, "a", false),
    ).toEqual({ matched: true, id: "c" });
  });

  it("maps Ctrl+number on non-mac and Cmd+number on mac; 9 is last", () => {
    expect(resolveSwitch(ev({ code: "Digit2", ctrlKey: true }), ids, "a", false)).toEqual({
      matched: true,
      id: "b",
    });
    // Ctrl+number is NOT a jump on mac (Cmd is the modifier there).
    expect(resolveSwitch(ev({ code: "Digit2", ctrlKey: true }), ids, "a", true).matched).toBe(
      false,
    );
    expect(resolveSwitch(ev({ code: "Digit2", metaKey: true }), ids, "a", true)).toEqual({
      matched: true,
      id: "b",
    });
    expect(resolveSwitch(ev({ code: "Digit9", ctrlKey: true }), ids, "a", false)).toEqual({
      matched: true,
      id: "c",
    });
  });

  it("recognises the mac-only bracket and arrow bindings, and only on mac", () => {
    expect(
      resolveSwitch(ev({ code: "BracketRight", metaKey: true, shiftKey: true }), ids, "a", true),
    ).toEqual({ matched: true, id: "b" });
    expect(
      resolveSwitch(ev({ key: "ArrowLeft", metaKey: true, altKey: true }), ids, "b", true),
    ).toEqual({ matched: true, id: "a" });
    // Not installed on non-mac.
    expect(
      resolveSwitch(ev({ code: "BracketRight", metaKey: true, shiftKey: true }), ids, "a", false)
        .matched,
    ).toBe(false);
  });

  it("ignores keys that are not a switch shortcut", () => {
    expect(resolveSwitch(ev({ key: "Tab" }), ids, "a", false).matched).toBe(false);
    expect(resolveSwitch(ev({ key: "a", ctrlKey: true }), ids, "a", false).matched).toBe(false);
  });
});

// ── The hook, in jsdom ──────────────────────────────────────────────────────

function mount(props: {
  ids: string[];
  activeId?: string;
  navigate: (to: string) => void;
}) {
  const Harness = () => {
    useTabHotkeys(props.ids, props.activeId, props.navigate);
    return null;
  };
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);
  act(() => root.render(React.createElement(Harness)));
  return () => {
    act(() => root.unmount());
    container.remove();
  };
}

function pressCtrlTab(shift = false) {
  const e = new KeyboardEvent("keydown", {
    key: "Tab",
    ctrlKey: true,
    shiftKey: shift,
    bubbles: true,
    cancelable: true,
  });
  document.body.dispatchEvent(e);
  return e;
}

describe("useTabHotkeys (desktop gating + capture-phase switch)", () => {
  afterEach(() => {
    delete (window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
    vi.restoreAllMocks();
  });

  it("switches to the next session and consumes the key when on desktop", () => {
    (window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ = {};
    const navigate = vi.fn();
    const unmount = mount({ ids: ["a", "b", "c"], activeId: "a", navigate });

    const e = pressCtrlTab();
    expect(navigate).toHaveBeenCalledWith("/sessions/b");
    expect(e.defaultPrevented).toBe(true); // consumed before xterm/the shell

    unmount();
  });

  it("installs no binding in the web build", () => {
    // No __TAURI_INTERNALS__ → isDesktop() is false.
    const navigate = vi.fn();
    const unmount = mount({ ids: ["a", "b", "c"], activeId: "a", navigate });

    const e = pressCtrlTab();
    expect(navigate).not.toHaveBeenCalled();
    expect(e.defaultPrevented).toBe(false); // the browser keeps its Ctrl+Tab

    unmount();
  });

  it("removes the listener on unmount", () => {
    (window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ = {};
    const navigate = vi.fn();
    const unmount = mount({ ids: ["a", "b", "c"], activeId: "a", navigate });
    unmount();

    pressCtrlTab();
    expect(navigate).not.toHaveBeenCalled();
  });
});
