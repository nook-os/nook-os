// MAIN-471: copy and markdown export for run transcripts.
//
// What is worth pinning: the markdown shape (sources as headers, content
// verbatim, ANSI stripped), and that the export always carries the FULL
// transcript even when the view folds part of it away — an incident paste
// that silently lost the agent narration would be worse than no button.
import React from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import type { LoopJobTranscriptEntry } from "@nookos/api";

import {
  fileSlug,
  TranscriptActions,
  transcriptMarkdown,
} from "./transcriptExport";

const line = (
  id: string,
  source: string,
  content: string,
): LoopJobTranscriptEntry =>
  ({
    id,
    job_id: "j1",
    source,
    content,
    at: "2026-08-09T00:00:00Z",
  }) as LoopJobTranscriptEntry;

beforeEach(() => cleanup());

describe("transcriptMarkdown", () => {
  it("headers each source once per run and strips ANSI, content verbatim", () => {
    const md = transcriptMarkdown([
      line("1", "system", "job started"),
      line("2", "agent", "\u001b[32mgreen\u001b[0m text"),
      line("3", "agent", "second agent line"),
      line("4", "system", "verdict: approved"),
    ]);
    expect(md).toBe(
      [
        "## system",
        "job started",
        "## agent",
        "green text",
        "second agent line",
        "## system",
        "verdict: approved",
      ].join("\n\n") + "\n",
    );
  });

  it("is empty for an empty transcript", () => {
    expect(transcriptMarkdown([])).toBe("");
  });
});

describe("fileSlug", () => {
  it("keeps keys and flattens everything else to dashes", () => {
    expect(fileSlug("MAIN-42")).toBe("MAIN-42");
    expect(fileSlug("Nook@OS-PR #12")).toBe("Nook-OS-PR-12");
    expect(fileSlug("  weird//name  ")).toBe("weird-name");
  });
});

describe("TranscriptActions", () => {
  it("copies the whole transcript as markdown", async () => {
    const writeText = vi.fn(async (_text: string) => {});
    Object.assign(navigator, { clipboard: { writeText } });

    render(
      <TranscriptActions
        lines={[line("1", "system", "hello")]}
        filename="x.md"
      />,
    );
    fireEvent.click(screen.getByTestId("transcript-copy"));
    // The handler routes through a promise chain (so a missing clipboard
    // rejects instead of throwing); flush it before asserting.
    await new Promise((r) => setTimeout(r, 0));
    expect(writeText).toHaveBeenCalledWith("## system\n\nhello\n");
  });

  it("downloads under the given filename", async () => {
    const clicks: string[] = [];
    URL.createObjectURL = vi.fn(() => "blob:fake");
    URL.revokeObjectURL = vi.fn();
    const click = vi
      .spyOn(HTMLAnchorElement.prototype, "click")
      .mockImplementation(function (this: HTMLAnchorElement) {
        clicks.push(this.download);
      });

    render(
      <TranscriptActions
        lines={[line("1", "system", "hello")]}
        filename="MAIN-42-abcd1234.md"
      />,
    );
    fireEvent.click(screen.getByTestId("transcript-download"));
    expect(clicks).toEqual(["MAIN-42-abcd1234.md"]);
    // The revoke is DEFERRED (Safari cancels a download whose URL is revoked
    // during click dispatch), so it lands on the next tick, not synchronously.
    expect(URL.revokeObjectURL).not.toHaveBeenCalled();
    await new Promise((r) => setTimeout(r, 0));
    expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:fake");
    click.mockRestore();
  });

  it("renders nothing for an empty transcript", () => {
    render(<TranscriptActions lines={[]} filename="x.md" />);
    expect(screen.queryByTestId("transcript-copy")).toBeNull();
  });
});
