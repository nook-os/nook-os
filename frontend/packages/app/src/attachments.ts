// The rules an attachment view needs, kept out of the component so they can be
// tested without a DOM (MAIN-533).

/** A size a person can read. Binary units, because that is what the upload cap
 *  is expressed in — a "30 MB" file refused by a 30 MiB limit reads as a bug. */
export function formatSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "—";
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let n = bytes / 1024;
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i += 1;
  }
  return `${n < 10 ? n.toFixed(1) : Math.round(n)} ${units[i]}`;
}

/**
 * Does this render as a picture?
 *
 * The claimed type, and only the plainly-spelled ones — the serving route makes
 * exactly this judgement about what it will send inline, and a preview for
 * something it will send as a download would be a broken image icon. SVG is
 * excluded here for a second reason: it is a document that can carry script,
 * and the one place it is safe is the sandboxed frame the server's CSP creates,
 * which an `<img>` is not.
 */
export function isImage(contentType: string): boolean {
  const base = (contentType.split(";")[0] ?? "").trim().toLowerCase();
  return ["image/png", "image/jpeg", "image/gif", "image/webp", "image/avif", "image/bmp"].includes(
    base,
  );
}
