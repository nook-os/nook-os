// The composer's Giphy search picker (MAIN-171 AC-2/AC-3/AC-4).
//
// It exists only when the deployment has a key: the caller renders nothing at
// all when `NOOK_GIPHY_KEY` is unset, so there is no disabled button, no
// "configure Giphy" nag and no failing request (AC-3). That is why the key is a
// required prop here rather than an optional one — a keyless GifPicker is not a
// state this component can be in.
//
// Picking a GIF posts its URL as a message; the message list recognises the URL
// and renders it inline (see `giphyGifUrl`). Nothing is uploaded (NG-1).
import React, { useCallback, useEffect, useRef, useState } from "react";
import { Loader2 } from "lucide-react";
import { searchGifs, type GiphyGif } from "@nookos/api";
import { useAnchoredMenu } from "./useAnchoredMenu";

/** Matches `.chat-gif-picker` in the stylesheet. */
const PICKER_HEIGHT = 340;
const PICKER_WIDTH = 320;

export interface GifPickerProps {
  /** The operator's Giphy API key. Its presence is what makes this render. */
  apiKey: string;
  /** The chosen GIF's URL, to be posted as a message body. */
  onPick: (url: string) => void;
  disabled?: boolean;
}

export function GifPicker({ apiKey, onPick, disabled = false }: GifPickerProps) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [gifs, setGifs] = useState<GiphyGif[]>([]);
  const [loading, setLoading] = useState(false);
  const [failed, setFailed] = useState(false);
  const trigger = useRef<HTMLButtonElement>(null);

  // Closing forgets the search. Otherwise a reopen paints the PREVIOUS query's
  // results until the trending request replaces them — and the "Searching…"
  // note, guarded on an empty grid, never shows while it does.
  const close = useCallback(() => {
    setOpen(false);
    setQuery("");
    setGifs([]);
  }, []);
  const menu = useAnchoredMenu(open, close, {
    height: PICKER_HEIGHT,
    width: PICKER_WIDTH,
  });

  const dismiss = useCallback(() => {
    close();
    trigger.current?.focus();
  }, [close]);

  // Escape closes from wherever focus is — same reason as the emoji picker's:
  // the panel is portalled to the body, so a handler on it only ever sees keys
  // typed inside it.
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.preventDefault();
      dismiss();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [open, dismiss]);

  // One search per word, not per keystroke: each keystroke re-runs this effect,
  // whose cleanup cancels the pending timer and aborts any request already in
  // flight — so a superseded reply can never land after the one that replaced
  // it. The empty query (trending, on open) fires at once; there is nothing to
  // wait for it to finish typing.
  useEffect(() => {
    if (!open) return;
    const ctl = new AbortController();
    setLoading(true);
    setFailed(false);
    const timer = setTimeout(
      () => {
        searchGifs(apiKey, query, 24, ctl.signal)
          .then((found) => {
            setGifs(found);
            setLoading(false);
          })
          .catch(() => {
            // An abort is the normal path while someone is still typing — not a
            // failure, and the search that replaced it owns the state now.
            if (ctl.signal.aborted) return;
            setFailed(true);
            setLoading(false);
          });
      },
      query.trim() ? 250 : 0,
    );
    return () => {
      clearTimeout(timer);
      ctl.abort();
    };
  }, [open, apiKey, query]);

  return (
    <div ref={menu.hostRef} className="chat-composer-wrap">
      <button
        ref={trigger}
        type="button"
        className={`chat-composer-btn gif${open ? " on" : ""}`}
        title="Post a GIF"
        aria-label="Post a GIF"
        aria-haspopup="dialog"
        aria-expanded={open}
        disabled={disabled}
        onClick={() => (open ? close() : setOpen(true))}
      >
        GIF
      </button>
      {menu.portal(
        <>
          <input
            className="chat-gif-search"
            type="search"
            autoFocus
            value={query}
            placeholder="Search GIPHY…"
            aria-label="Search GIPHY"
            onChange={(e) => setQuery(e.target.value)}
          />
          <div className="chat-gif-results">
            {failed ? (
              <div className="chat-gif-note">GIPHY could not be reached.</div>
            ) : loading && gifs.length === 0 ? (
              <div className="chat-gif-note">
                <Loader2 size={13} className="spin" /> Searching…
              </div>
            ) : gifs.length === 0 ? (
              <div className="chat-gif-note">No GIFs for that.</div>
            ) : (
              gifs.map((g) => (
                <button
                  key={g.id}
                  type="button"
                  className="chat-gif-opt"
                  aria-label={`Post ${g.title}`}
                  onClick={() => {
                    dismiss();
                    onPick(g.fullUrl);
                  }}
                >
                  <img src={g.previewUrl} alt={g.title} loading="lazy" />
                </button>
              ))
            )}
          </div>
          {/* AC-4: GIPHY's terms require their attribution mark wherever their
              results are shown. It sits in the picker's own chrome so it cannot
              scroll away with the grid. */}
          <a
            className="chat-gif-attribution"
            href="https://giphy.com"
            target="_blank"
            rel="noreferrer noopener"
          >
            Powered by GIPHY
          </a>
        </>,
        "chat-gif-picker",
        { role: "dialog", "aria-label": "GIPHY" },
      )}
    </div>
  );
}

// Recognising a posted GIF lives HERE rather than beside `searchGifs` in
// `@nookos/api`, on purpose: it is a rendering decision (how does the message
// list draw this body?), and ChatView must be able to make it in every surface
// that mounts it — including the several test files that replace the whole api
// module with a stub, which would otherwise have to keep growing a copy.
/**
 * The URL of the GIF a message body *is*, or `null` for an ordinary message.
 *
 * Deliberately narrow: the whole body must be one `https` URL on a giphy.com
 * host ending in an image extension. A chat surface that turned any URL into an
 * `<img>` would let anyone make every reader fetch a resource of their choosing
 * — so this recognises the thing this feature posts, and nothing else.
 */
export function giphyGifUrl(body: string): string | null {
  const text = body.trim();
  if (!text || /\s/.test(text)) return null;
  let url: URL;
  try {
    url = new URL(text);
  } catch {
    return null;
  }
  if (url.protocol !== "https:") return null;
  if (url.hostname !== "giphy.com" && !url.hostname.endsWith(".giphy.com")) return null;
  if (!/\.(gif|webp)$/i.test(url.pathname)) return null;
  return text;
}
