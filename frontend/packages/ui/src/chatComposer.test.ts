// The two pure decisions behind MAIN-171: where an inserted emoji lands, and
// what counts as a GIF message. Both are exported functions precisely so they
// can be asserted on directly — the DOM half (a real caret, a real <img>) is
// covered by ChatComposer.test.tsx in packages/app, which can render JSX.
import { describe, expect, it } from "vitest";
import { insertAt } from "./ChatView";
import { giphyGifUrl } from "./GifPicker";

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
