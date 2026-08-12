// A message's files, and the ones staged in the composer (MAIN-535).
//
// Backend-agnostic like the rest of `ChatView`: an attachment here is a name, a
// type, a size and a URL, so the same rendering serves a channel, a DM and a
// thread with nothing to keep in step (AC-4).
//
// Two shapes, and the discriminator is the CONTENT TYPE the uploader's browser
// claimed. An image previews inline and opens full size; everything else is a
// chip that downloads. The claim is not trusted for anything but this choice —
// what a byte stream is actually served AS is the control plane's decision, and
// a file lying about being a PNG renders as a broken picture rather than as
// anything that can run.
import React from "react";
import { File, Paperclip, X } from "lucide-react";

export interface ChatViewAttachment {
  id: string;
  filename: string;
  contentType: string;
  sizeBytes: number;
  /** Where the bytes are. */
  url: string;
}

/** One file waiting in the composer, before or during its upload (AC-2/AC-8). */
export interface StagedAttachment {
  /** Stable for the life of the staging row — the upload has no id until it
   *  lands, and a chip that renumbers cannot be removed reliably. */
  key: string;
  filename: string;
  sizeBytes: number;
  contentType: string;
  /** The upload is in flight. */
  uploading?: boolean;
  /** The user-content id, once it has landed. Absent while uploading. */
  contentId?: string;
}

/** Bytes as a person reads them. Deliberately 1024-based and one decimal at
 *  most: the number is a sanity check on a filename, not an accounting figure. */
export function formatSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "";
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

/** Does this claim to be a picture we can put in an `<img>`? */
export function isPreviewableImage(contentType: string): boolean {
  const base = contentType.split(";")[0]?.trim().toLowerCase() ?? "";
  // SVG is an image and the server sandboxes it, but an `<img>` in this origin
  // is not where a scriptable format belongs — it chips instead.
  return base.startsWith("image/") && base !== "image/svg+xml";
}

/** A posted message's files. Renders nothing at all when there are none, so
 *  every message that has never carried one is byte-for-byte unchanged. */
export function MessageAttachments({
  attachments,
}: {
  attachments: ChatViewAttachment[];
}) {
  if (attachments.length === 0) return null;
  return (
    <div className="chat-attachments">
      {attachments.map((a) =>
        isPreviewableImage(a.contentType) ? (
          <a
            key={a.id}
            className="chat-attachment-image"
            href={a.url}
            target="_blank"
            rel="noreferrer noopener"
            title={`${a.filename} · ${formatSize(a.sizeBytes)}`}
          >
            <img src={a.url} alt={a.filename} loading="lazy" />
          </a>
        ) : (
          <a
            key={a.id}
            className="chat-attachment-chip"
            href={a.url}
            // The server already answers with `Content-Disposition: attachment`
            // for anything it will not render; this only supplies the name the
            // browser should save it under.
            download={a.filename}
          >
            <File size={13} aria-hidden="true" />
            <span className="chat-attachment-name">{a.filename}</span>
            <span className="chat-attachment-meta">
              {shortType(a.contentType)} · {formatSize(a.sizeBytes)}
            </span>
          </a>
        ),
      )}
    </div>
  );
}

/** `application/zip` → `zip`; `image/png` → `png`. The full type is noise in a
 *  chip, and the subtype is the part a person recognises. */
function shortType(contentType: string): string {
  const base = contentType.split(";")[0]?.trim().toLowerCase() ?? "";
  const subtype = base.split("/")[1] ?? base;
  return subtype.replace(/^x-/, "").replace(/^vnd\..*\./, "") || "file";
}

/** The composer's staging row: what will be sent, and how to take it back off. */
export function StagedAttachments({
  staged,
  onRemove,
  notice,
  error,
}: {
  staged: StagedAttachment[];
  onRemove: (key: string) => void;
  /** Something a person must be able to read BEFORE sending — DM files are not
   *  encrypted (AC-7). Shown only when there is something staged, because that
   *  is the only moment it is about to become true. */
  notice?: string | null;
  /** An upload that failed or was refused (AC-8). Shown whether or not anything
   *  is staged: the refused file left nothing behind but this. */
  error?: string | null;
}) {
  if (staged.length === 0 && !error) return null;
  return (
    <div className="chat-staged">
      {staged.length > 0 && (
        <div className="chat-staged-row">
          {staged.map((s) => (
            <span
              key={s.key}
              className={`chat-staged-chip${s.uploading ? " uploading" : ""}`}
            >
              <Paperclip size={12} aria-hidden="true" />
              <span className="chat-attachment-name">{s.filename}</span>
              <span className="chat-attachment-meta">
                {s.uploading ? "uploading…" : formatSize(s.sizeBytes)}
              </span>
              <button
                type="button"
                className="chat-staged-remove"
                aria-label={`Remove ${s.filename}`}
                onClick={() => onRemove(s.key)}
              >
                <X size={12} aria-hidden="true" />
              </button>
            </span>
          ))}
        </div>
      )}
      {error && (
        <div className="chat-staged-error" role="alert">
          {error}
        </div>
      )}
      {notice && staged.length > 0 && <div className="chat-staged-notice">{notice}</div>}
    </div>
  );
}
