// The pure decisions behind the composer: where an inserted emoji lands and
// what counts as a GIF message (MAIN-171), and how typed text is read as a
// command (MAIN-529). All are exported functions precisely so they can be
// asserted on directly — the DOM halves (a real caret, a real <img>, a real
// palette) are covered by ChatComposer.test.tsx and ChatCommands.test.tsx in
// packages/app, which can render JSX.
import { describe, expect, it } from "vitest";
import {
  insertAt,
  matchCommands,
  paletteQuery,
  parseCommand,
  type ChatViewCommand,
} from "./ChatView";
import { giphyGifUrl } from "./GifPicker";

/** A stand-in for what the server hands the composer. The names are not this
 *  file's business — they are fixture data, and nothing outside a fixture or a
 *  test may name a command (MAIN-529 AC-2, guarded by serverOwnedCommands). */
const SERVER_SET: ChatViewCommand[] = [
  { name: "help", args_hint: null, description: "List the commands you can use here." },
  { name: "hedge", args_hint: "<text>", description: "A second h, so a filter has work." },
  { name: "me", args_hint: "<text>", description: "Post what you are doing as an action." },
];

describe("insertAt", () => {
  it("splices at a collapsed caret and reports where the caret lands", () => {
    const r = insertAt("hello world", 5, 5, "🎉");
    expect(r.text).toBe("hello🎉 world");
    // After the emoji, not at the end of the box — the caret is where typing
    // continues, which is the whole point of inserting at the cursor.
    expect(r.caret).toBe(5 + "🎉".length);
  });

  it("replaces a selection, as typing over one would", () => {
    const r = insertAt("hello world", 6, 11, "👋");
    expect(r.text).toBe("hello 👋");
    expect(r.caret).toBe(r.text.length);
  });

  it("appends at the end of an empty box", () => {
    expect(insertAt("", 0, 0, "🚀")).toEqual({ text: "🚀", caret: 2 });
  });

  it("clamps a caret that outran the text instead of producing undefined", () => {
    // A stale selection index (the box was cleared under it) must not splice
    // `undefined` into the draft.
    expect(insertAt("ab", 99, 99, "!").text).toBe("ab!");
    expect(insertAt("ab", -5, -5, "!").text).toBe("!ab");
  });
});

describe("giphyGifUrl", () => {
  it("recognises a lone Giphy image URL", () => {
    const url = "https://media3.giphy.com/media/abc123/giphy.gif?cid=x&rid=giphy.gif";
    expect(giphyGifUrl(url)).toBe(url);
    expect(giphyGifUrl(`  ${url}  `)).toBe(url);
    expect(giphyGifUrl("https://giphy.com/media/abc/200w.webp")).toBe(
      "https://giphy.com/media/abc/200w.webp",
    );
  });

  it("refuses anything that is not exactly one Giphy image", () => {
    // Ordinary chat, including chat that merely mentions a GIF.
    expect(giphyGifUrl("look at this https://media.giphy.com/media/a/giphy.gif")).toBeNull();
    expect(giphyGifUrl("hello")).toBeNull();
    expect(giphyGifUrl("")).toBeNull();
    // A Giphy page, not a Giphy image — an <img> would render nothing.
    expect(giphyGifUrl("https://giphy.com/gifs/some-slug-abc123")).toBeNull();
    // The load-bearing refusals: any other host, and any other scheme. Without
    // these, one message could make every reader in the channel fetch a
    // resource of the sender's choosing.
    expect(giphyGifUrl("https://evil.example.com/tracker.gif")).toBeNull();
    expect(giphyGifUrl("https://giphy.com.evil.example/x.gif")).toBeNull();
    expect(giphyGifUrl("http://media.giphy.com/media/a/giphy.gif")).toBeNull();
    expect(giphyGifUrl("javascript:alert(1)")).toBeNull();
  });
});

describe("paletteQuery (AC-3)", () => {
  it("opens on a LEADING slash and narrows as more is typed", () => {
    expect(paletteQuery("/")).toBe("");
    expect(paletteQuery("/he")).toBe("he");
  });

  it("opens on nothing else — a slash anywhere but the front is just text", () => {
    expect(paletteQuery("")).toBeNull();
    expect(paletteQuery("and/or")).toBeNull();
    expect(paletteQuery("look at ./run.sh")).toBeNull();
    expect(paletteQuery(" /help")).toBeNull();
  });

  it("closes once arguments start, so Enter can run the command (AC-4/AC-5)", () => {
    expect(paletteQuery("/me waves")).toBeNull();
    expect(paletteQuery("/me ")).toBeNull();
    expect(paletteQuery("/me\n")).toBeNull();
  });
});

describe("matchCommands (AC-3)", () => {
  it("prefix-matches the name, keeping the server's order", () => {
    expect(matchCommands(SERVER_SET, "").map((c) => c.name)).toEqual([
      "help",
      "hedge",
      "me",
    ]);
    expect(matchCommands(SERVER_SET, "he").map((c) => c.name)).toEqual(["help", "hedge"]);
    expect(matchCommands(SERVER_SET, "hel").map((c) => c.name)).toEqual(["help"]);
  });

  it("ignores case while typing, and offers nothing for a name nobody has", () => {
    expect(matchCommands(SERVER_SET, "HE").map((c) => c.name)).toEqual(["help", "hedge"]);
    expect(matchCommands(SERVER_SET, "nook-spec")).toEqual([]);
  });
});

describe("parseCommand (AC-5/AC-6)", () => {
  it("splits a listed name from the rest of the line", () => {
    expect(parseCommand("/hedge ok fine", SERVER_SET)).toEqual({
      name: "hedge",
      args: "ok fine",
    });
    // No argument is still an invocation — what the argument means is the
    // server's business, including whether it may be empty.
    expect(parseCommand("/help", SERVER_SET)).toEqual({ name: "help", args: "" });
  });

  it("passes through anything the server did not list — the regression risk", () => {
    // This is how `/nook-spec …` still reaches an agent verbatim.
    expect(parseCommand("/nook-spec MAIN-1 do a thing", SERVER_SET)).toBeNull();
    expect(parseCommand("/", SERVER_SET)).toBeNull();
    // Matched exactly, as the server matches it: a near miss is not a command.
    expect(parseCommand("/HELP", SERVER_SET)).toBeNull();
    expect(parseCommand("/helpful", SERVER_SET)).toBeNull();
  });

  it("passes through ordinary text, and text with a slash inside it", () => {
    expect(parseCommand("hello", SERVER_SET)).toBeNull();
    expect(parseCommand("and/or", SERVER_SET)).toBeNull();
  });

  it("finds nothing at all when the server offered nothing", () => {
    expect(parseCommand("/help", [])).toBeNull();
  });
});
