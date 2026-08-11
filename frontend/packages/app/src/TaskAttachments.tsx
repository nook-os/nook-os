// Files on a ticket and on its comments (MAIN-533).
//
// Three gestures put a file here — a drop, a picker, a paste — and they all end
// in the same two steps: upload the bytes, then record the join. Splitting them
// is what lets a paste into the comment box work at all: the comment does not
// exist yet when the image arrives, so its upload finishes and waits, and the
// join is written the moment the comment has an id.
//
// An upload in flight is therefore local state, not server state, and it
// survives its own failure on purpose (AC-5). The worst version of this feature
// is one where a network blip eats the paragraph somebody was writing.
import React, { useCallback, useEffect, useRef, useState } from "react";
import { Paperclip, RefreshCw, X } from "lucide-react";
import {
  contentNeedsFetch,
  messageFrom,
  uploadUserContent,
  userContentObjectUrl,
  userContentUrl,
  type TaskAttachment,
} from "@nookos/api";
import { formatSize, isImage } from "./attachments";

/** One file on its way to the store. `content` is set once the bytes land. */
export interface Upload {
  key: string;
  file: File;
  /** 0–1 while uploading. */
  progress: number;
  status: "uploading" | "failed" | "done";
  error?: string;
  contentId?: string;
}

let nextKey = 0;

/**
 * Upload files and tell the caller when each one's bytes have landed.
 *
 * `onUploaded` is where the join is written — to a ticket immediately, or to a
 * comment once it has been posted. The hook itself knows nothing about parents,
 * which is why one copy serves both.
 */
export function useUploads(onUploaded?: (contentId: string, file: File) => Promise<void> | void) {
  const [uploads, setUploads] = useState<Upload[]>([]);
  const aborts = useRef(new Map<string, () => void>());

  const patch = useCallback((key: string, next: Partial<Upload>) => {
    setUploads((prev) => prev.map((u) => (u.key === key ? { ...u, ...next } : u)));
  }, []);

  const run = useCallback(
    async (key: string, file: File) => {
      patch(key, { status: "uploading", progress: 0, error: undefined });
      const handle = uploadUserContent(file, (fraction) => patch(key, { progress: fraction }));
      aborts.current.set(key, handle.abort);
      try {
        const content = await handle.done;
        await onUploaded?.(content.id, file);
        // A finished upload leaves the tray: the attachment itself is now the
        // thing on screen, and a row saying "100%" beside it is noise.
        setUploads((prev) => prev.filter((u) => u.key !== key));
      } catch (e) {
        patch(key, {
          status: "failed",
          error: e instanceof Error ? e.message : messageFrom(0, ""),
        });
      } finally {
        aborts.current.delete(key);
      }
    },
    [onUploaded, patch],
  );

  const add = useCallback(
    (files: File[] | FileList) => {
      const list = Array.from(files);
      if (list.length === 0) return;
      const started = list.map((file) => ({
        key: `u${nextKey++}`,
        file,
        progress: 0,
        status: "uploading" as const,
      }));
      setUploads((prev) => [...prev, ...started]);
      for (const u of started) void run(u.key, u.file);
    },
    [run],
  );

  const retry = useCallback(
    (key: string) => {
      const u = uploads.find((x) => x.key === key);
      if (u) void run(key, u.file);
    },
    [run, uploads],
  );

  const dismiss = useCallback((key: string) => {
    aborts.current.get(key)?.();
    setUploads((prev) => prev.filter((u) => u.key !== key));
  }, []);

  return { uploads, add, retry, dismiss };
}

/** The tray of in-flight and failed uploads. A failure stays until it is dealt
 *  with, and dealing with it never touches what is being written (AC-5). */
export function UploadTray({
  uploads,
  onRetry,
  onDismiss,
}: {
  uploads: Upload[];
  onRetry: (key: string) => void;
  onDismiss: (key: string) => void;
}) {
  if (uploads.length === 0) return null;
  return (
    <div className="attach-tray">
      {uploads.map((u) => (
        <div key={u.key} className={`attach-upload${u.status === "failed" ? " failed" : ""}`}>
          <span className="attach-name" title={u.file.name}>
            {u.file.name}
          </span>
          {u.status === "failed" ? (
            <>
              <span className="attach-error">{u.error}</span>
              <button
                type="button"
                className="btn small"
                aria-label={`retry ${u.file.name}`}
                onClick={() => onRetry(u.key)}
              >
                <RefreshCw size={11} /> Retry
              </button>
            </>
          ) : (
            <progress
              className="attach-progress"
              aria-label={`uploading ${u.file.name}`}
              value={u.progress}
              max={1}
            />
          )}
          <button
            type="button"
            className="btn small"
            aria-label={`dismiss ${u.file.name}`}
            onClick={() => onDismiss(u.key)}
          >
            <X size={11} />
          </button>
        </div>
      ))}
    </div>
  );
}

/**
 * The bytes' URL, ready for an `<img>` or a link.
 *
 * Same-origin in a browser the session cookie already authenticates, so the
 * plain URL is both correct and cacheable. A desktop app authenticates with a
 * bearer token no tag can carry, so there — and only there — the bytes are
 * fetched and handed over as an object URL, revoked when the view goes.
 */
function useContentUrl(id: string): string | undefined {
  const [url, setUrl] = useState<string | undefined>(() =>
    contentNeedsFetch() ? undefined : userContentUrl(id),
  );
  useEffect(() => {
    if (!contentNeedsFetch()) {
      setUrl(userContentUrl(id));
      return;
    }
    let object: string | undefined;
    let live = true;
    void userContentObjectUrl(id).then(
      (u) => {
        if (!live) {
          URL.revokeObjectURL(u);
          return;
        }
        object = u;
        setUrl(u);
      },
      () => setUrl(undefined),
    );
    return () => {
      live = false;
      if (object) URL.revokeObjectURL(object);
    };
  }, [id]);
  return url;
}

/** One attached file: a picture, or a chip that downloads (AC-4). */
function Attachment({
  a,
  canRemove,
  onRemove,
}: {
  a: TaskAttachment;
  canRemove: boolean;
  onRemove: (id: string) => void;
}) {
  const url = useContentUrl(a.user_content_id);
  const remove = canRemove ? (
    <button
      type="button"
      className="attach-remove"
      title="remove this attachment"
      aria-label={`remove ${a.filename}`}
      onClick={() => onRemove(a.id)}
    >
      <X size={11} />
    </button>
  ) : null;

  if (isImage(a.content_type)) {
    return (
      <span className="attach-item image">
        {/* Full size is the original, opened in its own tab — there is no
            thumbnail to fall back from (NG-5). */}
        <a href={url} target="_blank" rel="noreferrer" title={`${a.filename} — open full size`}>
          <img className="attach-thumb" src={url} alt={a.filename} />
        </a>
        {remove}
      </span>
    );
  }
  return (
    <span className="attach-item">
      <a
        className="attach-chip"
        href={url}
        download={a.filename}
        title={`${a.filename} — ${a.content_type}, ${formatSize(a.size_bytes)}`}
      >
        <Paperclip size={11} />
        <span className="attach-name">{a.filename}</span>
        <span className="faint small">{formatSize(a.size_bytes)}</span>
      </a>
      {remove}
    </span>
  );
}

/** Every attachment on one parent. */
export function AttachmentList({
  attachments,
  canRemove,
  onRemove,
}: {
  attachments: TaskAttachment[];
  /** Whether THIS viewer may remove a given attachment — the server decides
   *  too, but offering a button that 403s is a worse way to find out (AC-6). */
  canRemove: (a: TaskAttachment) => boolean;
  onRemove: (id: string) => void;
}) {
  if (attachments.length === 0) return null;
  return (
    <div className="attach-list">
      {attachments.map((a) => (
        <Attachment key={a.id} a={a} canRemove={canRemove(a)} onRemove={onRemove} />
      ))}
    </div>
  );
}

/** Wraps a region so dropping files on it uploads them (AC-3). */
export function DropZone({
  onFiles,
  className,
  children,
}: {
  onFiles: (files: FileList | File[]) => void;
  className?: string;
  children: React.ReactNode;
}) {
  const [over, setOver] = useState(false);
  // Counted rather than a boolean: `dragleave` fires when the pointer crosses
  // into a CHILD element, so a flag would flicker the highlight off over every
  // nested node the file passes above.
  const depth = useRef(0);
  return (
    <div
      className={`${className ?? ""}${over ? " drop-over" : ""}`}
      onDragEnter={(e) => {
        if (!hasFiles(e)) return;
        e.preventDefault();
        depth.current += 1;
        setOver(true);
      }}
      onDragOver={(e) => {
        if (!hasFiles(e)) return;
        // Without this the browser navigates to the dropped file, which loses
        // the page and everything unsaved on it.
        e.preventDefault();
      }}
      onDragLeave={() => {
        depth.current = Math.max(0, depth.current - 1);
        if (depth.current === 0) setOver(false);
      }}
      onDrop={(e) => {
        if (!hasFiles(e)) return;
        e.preventDefault();
        depth.current = 0;
        setOver(false);
        onFiles(e.dataTransfer.files);
      }}
    >
      {children}
    </div>
  );
}

function hasFiles(e: React.DragEvent): boolean {
  return Array.from(e.dataTransfer?.types ?? []).includes("Files");
}

/** The files in a paste, or none — a paste of text must fall through to the
 *  editor untouched (NG-6). */
export function pastedFiles(data: DataTransfer | null): File[] {
  if (!data) return [];
  return Array.from(data.items)
    .filter((i) => i.kind === "file")
    .map((i) => i.getAsFile())
    .filter((f): f is File => f !== null);
}

/** A file picker that looks like the rest of the chrome (AC-3). */
export function AttachButton({ onFiles }: { onFiles: (files: FileList) => void }) {
  const input = useRef<HTMLInputElement>(null);
  return (
    <>
      <button
        type="button"
        className="btn small"
        title="attach a file"
        onClick={() => input.current?.click()}
      >
        <Paperclip size={11} /> Attach
      </button>
      <input
        ref={input}
        type="file"
        multiple
        aria-label="attach a file"
        style={{ display: "none" }}
        onChange={(e) => {
          if (e.target.files?.length) onFiles(e.target.files);
          // Cleared so choosing the SAME file twice in a row still fires.
          e.target.value = "";
        }}
      />
    </>
  );
}
