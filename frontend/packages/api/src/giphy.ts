// Giphy (MAIN-171): searching for a GIF, and recognising one that was posted.
//
// The only third-party client in this package. It talks to api.giphy.com
// directly with the operator's own key — Giphy issue web keys for exactly this
// (their JS SDK embeds one in the page), so proxying the search through the
// control plane would add a hop without adding a secret.
//
// The key is never a literal here. It arrives from `/api/v1/config`, which
// reads `NOOK_GIPHY_KEY`; with no key the caller renders no GIF affordance at
// all and nothing in this file is ever reached.

/** One search hit, reduced to what the picker draws and what gets posted. */
export interface GiphyGif {
  id: string;
  /** Alt text. Giphy's own title, which is often empty. */
  title: string;
  /** The small, fixed-height still-sized animation the grid shows. */
  previewUrl: string;
  /** The URL posted as the message body, and opened by "click for full". */
  fullUrl: string;
}

const SEARCH_URL = "https://api.giphy.com/v1/gifs/search";
const TRENDING_URL = "https://api.giphy.com/v1/gifs/trending";

/** Giphy's response, narrowed to the two renditions we ask for. */
interface GiphyApiGif {
  id?: string;
  title?: string;
  images?: {
    fixed_height_small?: { url?: string };
    downsized_medium?: { url?: string };
    original?: { url?: string };
  };
}

function toGif(g: GiphyApiGif): GiphyGif | null {
  const full = g.images?.downsized_medium?.url ?? g.images?.original?.url;
  const preview = g.images?.fixed_height_small?.url ?? full;
  if (!g.id || !full || !preview) return null;
  return { id: g.id, title: g.title?.trim() || "GIF", previewUrl: preview, fullUrl: full };
}

/**
 * Search Giphy, or — for an empty query — show what is trending, so the picker
 * has something to draw the moment it opens.
 *
 * `signal` aborts an in-flight search when the person keeps typing; an aborted
 * fetch rejects, which the caller distinguishes from a real failure.
 */
export async function searchGifs(
  apiKey: string,
  query: string,
  limit = 24,
  signal?: AbortSignal,
): Promise<GiphyGif[]> {
  const q = query.trim();
  const url = new URL(q ? SEARCH_URL : TRENDING_URL);
  url.searchParams.set("api_key", apiKey);
  if (q) url.searchParams.set("q", q);
  url.searchParams.set("limit", String(limit));
  // `g` is Giphy's General audience rating — the strictest they offer.
  url.searchParams.set("rating", "g");
  url.searchParams.set("bundle", "messaging_non_clips");

  const res = await fetch(url.toString(), { signal });
  if (!res.ok) throw new Error(`giphy search failed: ${res.status} ${res.statusText}`);
  const body = (await res.json()) as { data?: GiphyApiGif[] };
  return (body.data ?? []).map(toGif).filter((g): g is GiphyGif => g !== null);
}
