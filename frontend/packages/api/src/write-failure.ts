// A single place a failed write is reported, so any client — the openapi-fetch
// `api` client or the hand-rolled chat client — funnels through the same "a
// thing you thought you did did not happen" path, and a new call site cannot
// forget to. See `setWriteFailureHandler` for why writes get this and reads do
// not.

/** A write that did not happen, and why — as much as we can say. */
export interface WriteFailure {
  method: string;
  path: string;
  /** Absent when the request never got a reply at all. */
  status?: number;
  message: string;
}

let handler: ((f: WriteFailure) => void) | null = null;

/**
 * Be told when a write fails, so something can say so.
 *
 * `openapi-fetch` returns errors rather than throwing, and almost every call
 * site reads `data` and ignores `error`. That is survivable one call at a time
 * and disastrous in aggregate: when a bug stopped the desktop app writing
 * anything at all, not one screen said so — a total write outage looked like
 * buttons that did nothing. Reporting centrally means a new call site cannot
 * forget, because it never had to remember.
 *
 * Reads are left alone: those have a query layer with error states, whereas a
 * failed write is a thing the person believes they just did.
 */
export function setWriteFailureHandler(
  fn: ((f: WriteFailure) => void) | null,
): void {
  handler = fn;
}

/** Report a failed write to the registered handler, if any. */
export function reportWriteFailure(f: WriteFailure): void {
  if (f.status === 401) return; // the auth gate already bounces to sign-in.
  handler?.(f);
}
