// The text half of `@` mentions (MAIN-633), asserted on strings and trees.
//
// These are the functions the picker and the renderer are BUILT from, and the
// ones that have to keep agreeing with the server's `mentioned_slugs` — so they
// are pinned here, where a failure names the rule that broke rather than a
// component that happened to notice.
import { describe, expect, it } from "vitest";
import {
  applyMention,
  mentionTrigger,
  scanMentions,
  splitMentions,
} from "./mentions";

describe("scanMentions", () => {
  it("finds each mention in order, folded the way the server folds it", () => {
    expect(scanMentions("Wire @Nook-Web against @nook-api's endpoint.")).toEqual([
      { from: 5, to: 14, slug: "nook-web" },
      { from: 23, to: 32, slug: "nook-api" },
    ]);
  });

  // The server's rule, and the reason it exists: without it every quoted email
  // address in a body mentions its own domain.
  it("does not read an email address as a mention", () => {
    expect(scanMentions("Ask dev@nookos.local about it.")).toEqual([]);
  });

  it("stops a slug at punctuation and ignores a bare @", () => {
    expect(scanMentions("(@web), then @").map((s) => s.slug)).toEqual(["web"]);
  });
});

describe("mentionTrigger", () => {
  it("opens on a bare @ with an empty query", () => {
    expect(mentionTrigger("see @", 5)).toEqual({ from: 4, to: 5, query: "" });
  });

  it("narrows as more is typed", () => {
    expect(mentionTrigger("see @noo", 8)).toEqual({ from: 4, to: 8, query: "noo" });
  });

  it("is closed when the caret is not in a mention at all", () => {
    expect(mentionTrigger("plain prose", 5)).toBeNull();
    expect(mentionTrigger("dev@nookos", 10)).toBeNull();
    expect(mentionTrigger("see @web and more", 12)).toBeNull();
  });

  // Popping a menu over a written reference somebody is CORRECTING is how an
  // autocomplete becomes something to fight.
  it("is closed mid-token, when the caret is not at the end", () => {
    expect(mentionTrigger("see @nook-web", 7)).toBeNull();
  });
});

describe("applyMention", () => {
  it("replaces what was typed with the slug and a space, caret past it", () => {
    const doc = "see @noo";
    const trigger = mentionTrigger(doc, 8)!;
    const edit = applyMention(doc, trigger, "nook-web");
    expect(edit.doc).toBe("see @nook-web ");
    expect(edit.from).toBe(edit.to);
    expect(edit.doc.slice(0, edit.from)).toBe("see @nook-web ");
  });

  it("keeps the rest of the line and does not double a space that is there", () => {
    const doc = "see @noo and stop";
    const edit = applyMention(doc, mentionTrigger(doc, 8)!, "nook-web");
    expect(edit.doc).toBe("see @nook-web and stop");
  });
});

describe("splitMentions", () => {
  const links = [{ slug: "nook-web", href: "/workspaces/w1" }];

  it("links a resolved slug and leaves the surrounding text alone", () => {
    expect(splitMentions("see @nook-web now", links)).toEqual([
      { type: "text", value: "see " },
      {
        type: "link",
        url: "/workspaces/w1",
        title: null,
        data: { hProperties: { className: "mention-link" } },
        children: [{ type: "text", value: "@nook-web" }],
      },
      { type: "text", value: " now" },
    ]);
  });

  // AC-5's other half, and the one that matters: an unresolved slug must not
  // become a link to nowhere, so the node is left for the caller to keep.
  it("leaves an unresolved slug entirely alone", () => {
    expect(splitMentions("see @not-a-repo now", links)).toBeNull();
  });
});
