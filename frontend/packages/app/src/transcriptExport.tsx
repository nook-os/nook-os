// Copy and markdown export for run transcripts (MAIN-471).
//
// The client already holds every line it renders, so export is a pure
// reshaping — no endpoint (NG-2), no PDF (NG-1). The export ALWAYS carries the
// full transcript: the fold is a display concern, and an incident paste that
// silently lost the folded agent narration would be worse than no button.
import React, { useState } from "react";
import { Check, Copy, Download, X } from "lucide-react";
import type { LoopJobTranscriptEntry } from "@nookos/api";
import { stripAnsi } from "./loop";

/** The transcript as clean markdown: sources as headers, content verbatim
 *  (ANSI stripped — the stored line keeps its escapes, the export is for
 *  humans). Consecutive lines from one source share a header, the way the
 *  chat view groups them. */
export function transcriptMarkdown(lines: LoopJobTranscriptEntry[]): string {
  const out: string[] = [];
  let last: string | null = null;
  for (const l of lines) {
    if (l.source !== last) {
      out.push(`## ${l.source}`);
      last = l.source;
    }
    out.push(stripAnsi(l.content));
  }
  return out.join("\n\n") + (out.length ? "\n" : "");
}

/** A filename fragment safe on every filesystem: whatever survives outside
 *  [A-Za-z0-9._-] collapses to one dash. */
export function fileSlug(s: string): string {
  return s.replace(/[^A-Za-z0-9._-]+/g, "-").replace(/^-+|-+$/g, "");
}

function download(filename: string, markdown: string) {
  const url = URL.createObjectURL(
    new Blob([markdown], { type: "text/markdown" }),
  );
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.click();
  // Deferred: Safari has historically cancelled a download whose URL was
  // revoked synchronously during click dispatch.
  setTimeout(() => URL.revokeObjectURL(url), 0);
}

/** The two controls, side by side wherever a transcript renders. `lines` is
 *  the FULL transcript, never the folded view (AC-3); a still-running job
 *  exports whatever exists so far. */
export function TranscriptActions({
  lines,
  filename,
}: {
  lines: LoopJobTranscriptEntry[];
  filename: string;
}) {
  const [copied, setCopied] = useState<"idle" | "copied" | "failed">("idle");
  if (lines.length === 0) return null;
  const flash = (state: "copied" | "failed") => {
    setCopied(state);
    setTimeout(() => setCopied("idle"), 1500);
  };
  return (
    <>
      <button
        className="btn small"
        title={
          copied === "failed"
            ? "copy failed — the clipboard is unavailable here"
            : "copy the whole transcript as markdown"
        }
        data-testid="transcript-copy"
        onClick={() => {
          // A silent failure pastes whatever was on the clipboard BEFORE —
          // worse than a visibly broken button for the incident-paste
          // workflow this exists for. Failure covers both a rejected write
          // and `navigator.clipboard` being absent (non-secure contexts).
          void Promise.resolve()
            .then(() => navigator.clipboard.writeText(transcriptMarkdown(lines)))
            .then(() => flash("copied"))
            .catch(() => flash("failed"));
        }}
      >
        {copied === "copied" ? (
          <Check size={11} />
        ) : copied === "failed" ? (
          <X size={11} />
        ) : (
          <Copy size={11} />
        )}{" "}
        {copied === "failed" ? "Copy failed" : "Copy"}
      </button>
      <button
        className="btn small"
        title={`download the transcript as ${filename}`}
        data-testid="transcript-download"
        onClick={() => download(filename, transcriptMarkdown(lines))}
      >
        <Download size={11} /> Export
      </button>
    </>
  );
}
