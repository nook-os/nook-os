// `@slug` in a description, as pure functions over text (MAIN-633).
//
// Two questions, one scanner. What is the caret sitting in — which decides
// whether the picker is open and what it is filtering on — and where are the
// mentions in a finished body, which decides what gets rendered as a link.
//
// The scanner deliberately agrees, character for character, with the server's
// `mentioned_slugs` (`crates/nook-control/src/services/workspace_refs.rs`): the
// text the editor completes is parsed and stored by that function, and a picker
// that offers a completion the parser will not recognise is worse than no
// picker. If one of them changes, both do.

import type { EditResult } from "./Markdown";

/** A slug found in some text, with the span the `@` and the token occupy. */
export interface MentionSpan {
  from: number;
  to: number;
  /** Lower-cased and stripped of trailing separators — what resolves. */
  slug: string;
}

const TOKEN = /[A-Za-z0-9_-]/;

/** `@` may only OPEN a word, so `dev@nookos.local` mentions nothing. */
function opensAWord(text: string, at: number): boolean {
  return at === 0 || !/[A-Za-z0-9]/.test(text[at - 1]);
}

/** The stored form of a typed token: lower case, no leading or trailing
 *  separator — `@web-` at the end of a clause is the same reference as `@web`. */
function fold(token: string): string {
  return token.toLowerCase().replace(/^[-_]+/, "").replace(/[-_]+$/, "");
}

/** Every `@slug` in `text`, in order. Overlapping is impossible: a token ends
 *  where its characters stop. */
export function scanMentions(text: string): MentionSpan[] {
  const out: MentionSpan[] = [];
  for (let i = 0; i < text.length; i++) {
    if (text[i] !== "@") continue;
    let j = i + 1;
    while (j < text.length && TOKEN.test(text[j])) j++;
    if (j > i + 1 && opensAWord(text, i)) {
      const slug = fold(text.slice(i + 1, j));
      if (slug) out.push({ from: i, to: j, slug });
    }
    i = Math.max(j - 1, i);
  }
  return out;
}

/** What the picker is completing, or `null` when the caret is not in a mention.
 *
 *  The caret must sit at the END of the token. Editing the middle of a written
 *  `@nook-web` is not an attempt to complete it, and popping a menu over the
 *  text somebody is fixing is how an autocomplete becomes something to fight. */
export interface MentionTrigger {
  /** Offset of the `@`. */
  from: number;
  /** Offset of the caret, which is the end of the token. */
  to: number;
  /** The letters typed so far, lower-cased. Empty for a bare `@`, which lists
   *  the first page of workspaces rather than nothing. */
  query: string;
}

export function mentionTrigger(doc: string, caret: number): MentionTrigger | null {
  if (caret < 1 || caret > doc.length) return null;
  // Mid-token: the caret is inside a word, not at the end of one.
  if (caret < doc.length && TOKEN.test(doc[caret])) return null;
  let i = caret;
  while (i > 0 && TOKEN.test(doc[i - 1])) i--;
  const at = i - 1;
  if (at < 0 || doc[at] !== "@" || !opensAWord(doc, at)) return null;
  return { from: at, to: caret, query: doc.slice(i, caret).toLowerCase() };
}

/** Replace the trigger's span with `@slug`, and put the caret past it.
 *
 *  A space follows the slug unless one already does. It is what a person would
 *  type next, and it is also what CLOSES the menu: the caret then sits after a
 *  space rather than at the end of a token, so `mentionTrigger` returns null on
 *  the very next update instead of re-opening the picker over its own result. */
export function applyMention(
  doc: string,
  trigger: MentionTrigger,
  slug: string,
): EditResult {
  const spaced = doc[trigger.to] === " " ? "" : " ";
  const insert = `@${slug}${spaced}`;
  const caret = trigger.from + insert.length;
  return {
    doc: doc.slice(0, trigger.from) + insert + doc.slice(trigger.to),
    from: caret,
    to: caret,
  };
}

/** A slug that resolved to a workspace, and where that workspace lives. */
export interface MentionLink {
  slug: string;
  href: string;
}

/** An mdast inline node, as much of one as this file needs to build. */
interface InlineNode {
  type: string;
  value?: string;
  url?: string;
  title?: string | null;
  /** `hProperties` is how mdast hands attributes to the HTML tree remark hands
   *  rehype — the only way to class a node built here rather than parsed. */
  data?: { hProperties?: Record<string, string> };
  children?: InlineNode[];
}

/** Split one text node around the mentions that resolve, or `null` when none
 *  do — in which case the caller leaves the node exactly as it was.
 *
 *  An UNRESOLVED `@word` is not in `links`, so it falls through as ordinary text
 *  (AC-5). That is the whole point: a typo must never render as a link to
 *  nowhere, and the reader has to be able to tell the difference. */
export function splitMentions(
  value: string,
  links: MentionLink[],
): InlineNode[] | null {
  const href = new Map(links.map((l) => [l.slug, l.href]));
  const hits = scanMentions(value).filter((s) => href.has(s.slug));
  if (!hits.length) return null;
  const out: InlineNode[] = [];
  let cursor = 0;
  for (const hit of hits) {
    if (hit.from > cursor) {
      out.push({ type: "text", value: value.slice(cursor, hit.from) });
    }
    out.push({
      type: "link",
      url: href.get(hit.slug)!,
      title: null,
      data: { hProperties: { className: "mention-link" } },
      // The written text, not the folded slug: `@Nook-Web` stays capitalised.
      children: [{ type: "text", value: value.slice(hit.from, hit.to) }],
    });
    cursor = hit.to;
  }
  if (cursor < value.length) {
    out.push({ type: "text", value: value.slice(cursor) });
  }
  return out;
}

/** Code is never a mention: `@web` inside a fence or backticks is a literal the
 *  author typed on purpose, and a link already has its own destination. */
const OPAQUE = new Set(["code", "inlineCode", "link", "linkReference", "html"]);

/** A remark plugin turning each resolved `@slug` into a link.
 *
 *  Hand-walked rather than `unist-util-visit`: replacing a node with SEVERAL
 *  nodes is a parent-level edit either way, and this keeps the renderer's
 *  dependency list as short as it is. */
export function remarkMentions(links: MentionLink[]) {
  return () => (tree: InlineNode) => {
    if (!links.length) return;
    const walk = (node: InlineNode) => {
      if (!node.children) return;
      const next: InlineNode[] = [];
      for (const child of node.children) {
        if (child.type === "text" && typeof child.value === "string") {
          const split = splitMentions(child.value, links);
          if (split) {
            next.push(...split);
            continue;
          }
        }
        if (!OPAQUE.has(child.type)) walk(child);
        next.push(child);
      }
      node.children = next;
    };
    walk(tree);
  };
}
