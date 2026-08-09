// The presence/typing model (MAIN-163 AC-4): local expiry, multi-typist
// wording, and the online set. Pure — no DOM, no socket, no clock of its own:
// every function takes `now`, so "four seconds later" is a number, not a wait.
import { describe, expect, it } from "vitest";
import {
  NO_TYPISTS,
  nextTypingExpiry,
  notePresence,
  noteTyping,
  pruneTyping,
  typingLabel,
  typistNames,
  TYPING_TTL_MS,
} from "./chatPresence";

const T0 = 1_700_000_000_000;

function typed(
  state = NO_TYPISTS,
  person: string,
  name: string | null,
  at: number,
  channel = "c1",
) {
  return noteTyping(state, { channel_id: channel, person, display_name: name }, at);
}

describe("typing expiry", () => {
  it("keeps a typist until the TTL and drops them after it", () => {
    const s = typed(NO_TYPISTS, "p-ada", "Ada", T0);
    expect(typistNames(s, "c1", T0 + TYPING_TTL_MS - 1)).toEqual(["Ada"]);
    expect(typistNames(s, "c1", T0 + TYPING_TTL_MS)).toEqual([]);
  });

  it("a fresh ping extends the same person rather than adding another", () => {
    let s = typed(NO_TYPISTS, "p-ada", "Ada", T0);
    s = typed(s, "p-ada", "Ada", T0 + 3000);
    expect(typistNames(s, "c1", T0 + TYPING_TTL_MS + 1)).toEqual(["Ada"]);
    expect(typistNames(s, "c1", T0 + 3000 + TYPING_TTL_MS)).toEqual([]);
  });

  it("pruning drops only the lapsed, and returns the same state when none did", () => {
    let s = typed(NO_TYPISTS, "p-ada", "Ada", T0);
    s = typed(s, "p-bob", "Bob", T0 + 2000);
    const unchanged = pruneTyping(s, T0 + 1000);
    expect(unchanged).toBe(s); // identity: a no-op sweep must not re-render
    const pruned = pruneTyping(s, T0 + TYPING_TTL_MS);
    expect(typistNames(pruned, "c1", T0 + TYPING_TTL_MS)).toEqual(["Bob"]);
  });

  it("reports the soonest expiry so a caller can wake exactly then", () => {
    expect(nextTypingExpiry(NO_TYPISTS)).toBeNull();
    let s = typed(NO_TYPISTS, "p-ada", "Ada", T0 + 2000);
    s = typed(s, "p-bob", "Bob", T0);
    expect(nextTypingExpiry(s)).toBe(T0 + TYPING_TTL_MS);
  });

  it("scopes typists to their own channel, and never echoes the viewer", () => {
    let s = typed(NO_TYPISTS, "p-ada", "Ada", T0, "c1");
    s = typed(s, "p-bob", "Bob", T0, "c2");
    expect(typistNames(s, "c1", T0)).toEqual(["Ada"]);
    expect(typistNames(s, "c2", T0)).toEqual(["Bob"]);
    expect(typistNames(s, null, T0)).toEqual([]);
    // The server fans a typing frame to every subscriber — the typist included.
    expect(typistNames(s, "c1", T0, "p-ada")).toEqual([]);
  });

  it("names a typist whose user row carries no display name", () => {
    const s = typed(NO_TYPISTS, "p-ghost", null, T0);
    expect(typistNames(s, "c1", T0)).toEqual(["Someone"]);
  });
});

describe("multi-typist rendering", () => {
  it("words one, two and three typists", () => {
    expect(typingLabel([])).toBeNull();
    expect(typingLabel(["Ada"])).toBe("Ada is typing…");
    expect(typingLabel(["Ada", "Bob"])).toBe("Ada and Bob are typing…");
    expect(typingLabel(["Ada", "Bob", "Cy"])).toBe("Ada, Bob and Cy are typing…");
  });

  it("counts a crowd instead of listing it", () => {
    expect(typingLabel(["Ada", "Bob", "Cy", "Dee"])).toBe("4 people are typing…");
  });

  it("renders typists in first-seen order, unchanged by a later ping", () => {
    let s = typed(NO_TYPISTS, "p-ada", "Ada", T0);
    s = typed(s, "p-bob", "Bob", T0 + 100);
    s = typed(s, "p-ada", "Ada", T0 + 200);
    expect(typingLabel(typistNames(s, "c1", T0 + 300))).toBe("Ada and Bob are typing…");
  });
});

describe("presence transitions", () => {
  it("adds on online and removes on offline", () => {
    const off: ReadonlySet<string> = new Set();
    const on = notePresence(off, { person: "p-ada", online: true });
    expect(on.has("p-ada")).toBe(true);
    expect(notePresence(on, { person: "p-ada", online: false }).has("p-ada")).toBe(false);
  });

  it("returns the same set when the frame said nothing new", () => {
    const on = notePresence(new Set(), { person: "p-ada", online: true });
    expect(notePresence(on, { person: "p-ada", online: true })).toBe(on);
    const off: ReadonlySet<string> = new Set();
    expect(notePresence(off, { person: "p-ada", online: false })).toBe(off);
  });

  it("tracks people independently", () => {
    let s = notePresence(new Set(), { person: "p-ada", online: true });
    s = notePresence(s, { person: "p-bob", online: true });
    s = notePresence(s, { person: "p-ada", online: false });
    expect([...s]).toEqual(["p-bob"]);
  });
});
