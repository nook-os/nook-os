// Uploading a file, and reading the bytes back (MAIN-532/533).
//
// Not `openapi-fetch`, and not `fetch` at all: `fetch` cannot report how much
// of a request body has gone out, so an upload through it is a spinner with no
// end in sight. `XMLHttpRequest` still owns upload progress in every browser,
// which is the whole reason this file exists beside the generated client.
//
// It also owns the error contract for uploads. Every failure the store can
// raise — over the cap, refused, unreachable — comes back as the same
// `{"error": …}` body the rest of the API uses, and the job here is to turn
// that into a sentence rather than let a raw JSON blob reach a person
// (MAIN-533 AC-9).
import { apiUrl, authHeaders, getEndpoint } from "./endpoint";
import type { components } from "./generated/schema";

export type UserContent = components["schemas"]["UserContent"];

export interface UploadHandle {
  /** Resolves with the stored record, rejects with a readable `Error`. */
  done: Promise<UserContent>;
  /** Give up on an upload in flight. Its promise rejects as `cancelled`. */
  abort: () => void;
}

/** Where the bytes for a stored file are. Same-origin in a browser, absolute
 *  when the client is pointed at a remote control plane. */
export function userContentUrl(id: string): string {
  return apiUrl(`/api/v1/user-content/${id}`);
}

/**
 * True when an `<img src>` or a plain link would reach the API unauthenticated.
 *
 * A browser on the same origin sends its session cookie with a subresource
 * request, so the URL alone is enough. A desktop app authenticates with a
 * bearer token, which no `<img>` tag can carry — there, the bytes have to be
 * fetched and handed over as an object URL instead.
 */
export function contentNeedsFetch(): boolean {
  return getEndpoint().token !== "";
}

/** Fetch stored bytes as an object URL, for the token-authenticated case.
 *  The caller owns the URL and must `URL.revokeObjectURL` it. */
export async function userContentObjectUrl(id: string): Promise<string> {
  const res = await fetch(userContentUrl(id), {
    credentials: "include",
    headers: authHeaders(),
  });
  if (!res.ok) throw new Error(await readError(res.status, await res.text()));
  return URL.createObjectURL(await res.blob());
}

/**
 * Send one file to the user-content store, reporting progress as it goes.
 *
 * Returns a handle rather than a bare promise because an upload is the one
 * request a person may want to take back — a 30 MB video attached by mistake
 * should stop, not finish and then be deleted.
 */
export function uploadUserContent(
  file: File,
  onProgress?: (fraction: number) => void,
): UploadHandle {
  const xhr = new XMLHttpRequest();
  const done = new Promise<UserContent>((resolve, reject) => {
    xhr.open("POST", apiUrl("/api/v1/user-content"));
    xhr.withCredentials = true;
    for (const [k, v] of Object.entries(authHeaders())) xhr.setRequestHeader(k, v);

    xhr.upload.onprogress = (e) => {
      // `lengthComputable` is false for a body of unknown size; reporting a
      // made-up fraction there would be a progress bar that lies.
      if (e.lengthComputable && e.total > 0) onProgress?.(e.loaded / e.total);
    };
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        onProgress?.(1);
        try {
          resolve(JSON.parse(xhr.responseText) as UserContent);
        } catch {
          reject(new Error("the upload finished but the server's answer was unreadable"));
        }
        return;
      }
      reject(new Error(messageFrom(xhr.status, xhr.responseText)));
    };
    // The request never left — offline, or a webview that refused the body.
    xhr.onerror = () =>
      reject(new Error("the upload could not reach the server — check your connection"));
    xhr.onabort = () => reject(new Error("cancelled"));

    const form = new FormData();
    form.append("file", file, file.name);
    xhr.send(form);
  });
  return { done, abort: () => xhr.abort() };
}

async function readError(status: number, body: string): Promise<string> {
  return messageFrom(status, body);
}

/**
 * The server's sentence, or one written here.
 *
 * Exported because it is the rule AC-9 is about, and a rule worth testing on
 * its own: whatever comes back, a person sees prose. A body that is not the
 * API's `{"error": …}` shape — a proxy's HTML, an empty 502 — must not be
 * printed at a person either, so the status is described instead.
 */
export function messageFrom(status: number, body: string): string {
  try {
    const parsed = JSON.parse(body) as { error?: string; message?: string };
    const said = parsed.error ?? parsed.message;
    if (typeof said === "string" && said.trim()) return said;
  } catch {
    // Not JSON. Fall through to the status.
  }
  if (status === 413) return "that file is too large to upload";
  if (status === 401 || status === 403) return "you are not allowed to upload here";
  if (status === 0) return "the upload could not reach the server — check your connection";
  if (status >= 500) return "the file store is unavailable — try again in a moment";
  return `the upload failed (${status})`;
}
