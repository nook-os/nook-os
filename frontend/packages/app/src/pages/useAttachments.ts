// Staging files for a message (MAIN-535), shared by the channel composer and
// the thread panel's so the two cannot drift.
//
// The upload happens the moment a file is chosen, dropped or pasted — not on
// send. That is what makes AC-8 answerable: a file over the cap is refused
// while the person is still composing, they read why in the box, and no message
// was posted to be broken by it. By the time Send is pressed every attachment
// is already a content id.
//
// The client is `@nookos/api`'s `uploadUserContent` — the SAME one ticket
// attachments use (MAIN-533). One store, one client, two consumers, which is
// what MAIN-532 built it for; a second copy here would be a second place for
// the error wording to drift. Its handle also offers per-file progress and
// abort, which this composer does not wire yet.
import { useCallback, useRef, useState } from "react";
import { uploadUserContent } from "@nookos/api";
import type { StagedAttachment } from "@nookos/ui";

export interface Attachments {
  staged: StagedAttachment[];
  error: string | null;
  /** Take files from the picker, a drop or a paste. */
  add: (files: File[]) => void;
  remove: (key: string) => void;
  /** The ids to post, in staged order — or `null` while any upload is still in
   *  flight, which is NOT the same answer as "nothing is attached".
   *
   *  It used to be `[]` for both, and a caller could not tell them apart: a
   *  send that slipped past the composer's guard read the empty array as "no
   *  files", posted text only, and dropped the staged uploads without a word.
   *  `null` makes a caller handle it or crash, which is the point. */
  ids: () => string[] | null;
  /** Called after a successful send — the message owns them now. */
  clear: () => void;
}

export function useAttachments(): Attachments {
  const [staged, setStaged] = useState<StagedAttachment[]>([]);
  const [error, setError] = useState<string | null>(null);
  const counter = useRef(0);

  const add = useCallback((files: File[]) => {
    setError(null);
    for (const file of files) {
      const key = `staged-${counter.current++}`;
      setStaged((prev) => [
        ...prev,
        {
          key,
          filename: file.name,
          sizeBytes: file.size,
          contentType: file.type || "application/octet-stream",
          uploading: true,
        },
      ]);
      // `.done` rather than the call itself: the handle carries an `abort` for
      // an upload a person takes back, which nothing here uses yet.
      void uploadUserContent(file)
        .done.then((content) => {
          setStaged((prev) =>
            prev.map((s) =>
              s.key === key
                ? {
                    ...s,
                    uploading: false,
                    contentId: content.id,
                    // The server's record wins: it computed the size it
                    // actually stored, and echoed back the name it recorded.
                    filename: content.filename,
                    sizeBytes: content.size_bytes,
                    contentType: content.content_type,
                  }
                : s,
            ),
          );
        })
        .catch((err: unknown) => {
          // The refused file leaves NOTHING staged — a chip that can never
          // become a content id is a message the composer would refuse to send
          // with no way to fix it but noticing the chip.
          setStaged((prev) => prev.filter((s) => s.key !== key));
          setError(
            `${file.name}: ${err instanceof Error ? err.message : String(err)}`,
          );
        });
    }
  }, []);

  const remove = useCallback((key: string) => {
    setStaged((prev) => prev.filter((s) => s.key !== key));
    setError(null);
  }, []);

  const ids = useCallback(
    () =>
      staged.every((s) => s.contentId)
        ? staged.map((s) => s.contentId as string)
        : null,
    [staged],
  );

  const clear = useCallback(() => {
    setStaged([]);
    setError(null);
  }, []);

  return { staged, error, add, remove, ids, clear };
}
