// MAIN-533: the three ways a file gets attached, and what happens when one
// fails. jsdom, with the upload client mocked — what is under test is the
// component's behaviour around an upload, not XMLHttpRequest.
import React from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

const uploads = vi.hoisted(() => ({
  calls: [] as File[],
  resolve: undefined as ((v: { id: string }) => void) | undefined,
  reject: undefined as ((e: Error) => void) | undefined,
  progress: undefined as ((f: number) => void) | undefined,
}));

vi.mock("@nookos/api", () => ({
  contentNeedsFetch: () => false,
  userContentUrl: (id: string) => `/api/v1/user-content/${id}`,
  userContentObjectUrl: async (id: string) => `blob:${id}`,
  messageFrom: () => "the upload failed",
  uploadUserContent: (file: File, onProgress?: (f: number) => void) => {
    uploads.calls.push(file);
    uploads.progress = onProgress;
    return {
      done: new Promise<{ id: string }>((res, rej) => {
        uploads.resolve = res as (v: { id: string }) => void;
        uploads.reject = rej;
      }),
      abort: () => {},
    };
  },
}));

import {
  AttachButton,
  AttachmentList,
  DropZone,
  UploadTray,
  pastedFiles,
  useUploads,
} from "./TaskAttachments";

afterEach(() => {
  cleanup();
  uploads.calls = [];
  uploads.resolve = undefined;
  uploads.reject = undefined;
});

function file(name: string, type: string) {
  return new File(["bytes"], name, { type });
}

/** A drop event jsdom will carry a file list on. */
function dropEvent(files: File[]) {
  return {
    dataTransfer: { files, items: [], types: ["Files"] },
  };
}

/** The component under test, wired the way TaskDetail wires it. */
function Harness({ onUploaded }: { onUploaded?: (id: string) => void }) {
  const u = useUploads((id) => onUploaded?.(id));
  return (
    <DropZone className="task-main" onFiles={u.add}>
      <AttachButton onFiles={u.add} />
      <UploadTray uploads={u.uploads} onRetry={u.retry} onDismiss={u.dismiss} />
    </DropZone>
  );
}

describe("attaching (AC-3)", () => {
  it("a dropped file starts an upload and shows progress", async () => {
    render(<Harness />);
    fireEvent.drop(document.querySelector(".task-main")!, dropEvent([file("a.png", "image/png")]));

    await waitFor(() => expect(uploads.calls.map((f) => f.name)).toEqual(["a.png"]));
    expect(screen.getByLabelText("uploading a.png")).toBeTruthy();
  });

  it("the picker starts the same upload", async () => {
    render(<Harness />);
    const input = screen.getByLabelText("attach a file") as HTMLInputElement;
    Object.defineProperty(input, "files", { value: [file("b.zip", "application/zip")] });
    fireEvent.change(input);
    await waitFor(() => expect(uploads.calls.map((f) => f.name)).toEqual(["b.zip"]));
  });

  it("a paste yields its files and ignores plain text", () => {
    const img = file("shot.png", "image/png");
    const data = {
      items: [
        { kind: "string", getAsFile: () => null },
        { kind: "file", getAsFile: () => img },
      ],
    } as unknown as DataTransfer;
    expect(pastedFiles(data)).toEqual([img]);
    expect(pastedFiles(null)).toEqual([]);
  });

  it("reports the finished content id once, then clears the tray", async () => {
    const seen: string[] = [];
    render(<Harness onUploaded={(id) => seen.push(id)} />);
    fireEvent.drop(document.querySelector(".task-main")!, dropEvent([file("a.png", "image/png")]));
    await waitFor(() => expect(uploads.resolve).toBeTruthy());
    uploads.resolve!({ id: "content-1" });
    await waitFor(() => expect(seen).toEqual(["content-1"]));
    expect(screen.queryByLabelText("uploading a.png")).toBeNull();
  });
});

describe("a failed upload (AC-5)", () => {
  it("says why, and offers retry and dismiss", async () => {
    render(<Harness />);
    fireEvent.drop(document.querySelector(".task-main")!, dropEvent([file("a.png", "image/png")]));
    await waitFor(() => expect(uploads.reject).toBeTruthy());
    uploads.reject!(new Error("that file is larger than the 30 MiB upload limit"));

    // The server's sentence, not a status code and not raw JSON (AC-9).
    await screen.findByText("that file is larger than the 30 MiB upload limit");

    fireEvent.click(screen.getByLabelText("retry a.png"));
    await waitFor(() => expect(uploads.calls.length).toBe(2));

    fireEvent.click(screen.getByLabelText("dismiss a.png"));
    await waitFor(() => expect(screen.queryByLabelText("dismiss a.png")).toBeNull());
  });
});

describe("rendering an attachment (AC-4)", () => {
  const png = {
    id: "att-1",
    parent_kind: "task",
    parent_id: "t1",
    attached_by: "u1",
    user_content_id: "c1",
    filename: "shot.png",
    content_type: "image/png",
    size_bytes: 2048,
    created_at: "2026-08-11T00:00:00Z",
  };
  const zip = { ...png, id: "att-2", user_content_id: "c2", filename: "logs.zip", content_type: "application/zip", size_bytes: 30 * 1024 * 1024 };

  it("previews an image and chips everything else", () => {
    render(
      <AttachmentList attachments={[png, zip]} canRemove={() => false} onRemove={() => {}} />,
    );
    const img = screen.getByAltText("shot.png") as HTMLImageElement;
    expect(img.getAttribute("src")).toBe("/api/v1/user-content/c1");
    // Full size on click — the original, opened in its own tab.
    expect(img.closest("a")!.getAttribute("href")).toBe("/api/v1/user-content/c1");

    const chip = screen.getByText("logs.zip").closest("a") as HTMLAnchorElement;
    expect(chip.getAttribute("download")).toBe("logs.zip");
    expect(screen.getByText("30 MiB")).toBeTruthy();
    expect(screen.queryByAltText("logs.zip")).toBeNull();
  });

  it("offers removal only to whoever may remove it (AC-6)", () => {
    const removed: string[] = [];
    const { rerender } = render(
      <AttachmentList attachments={[png]} canRemove={() => false} onRemove={(id) => removed.push(id)} />,
    );
    expect(screen.queryByLabelText("remove shot.png")).toBeNull();

    rerender(
      <AttachmentList attachments={[png]} canRemove={() => true} onRemove={(id) => removed.push(id)} />,
    );
    fireEvent.click(screen.getByLabelText("remove shot.png"));
    expect(removed).toEqual(["att-1"]);
  });
});
