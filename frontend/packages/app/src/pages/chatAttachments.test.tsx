// MAIN-535 on the client: the three ways a file gets into a message, what a
// message's files look like once posted, and the two refusals — an upload over
// the cap, and an empty box with nothing to say.
//
// Driven through the same fake data source `ChatView.test.tsx` uses: plain
// arrays and `vi.fn`, no chat service. The staging half is driven through
// `useAttachments`, which owns the upload, with the upload itself mocked — the
// question here is what the composer does with a success and with a refusal.
import React from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderHook } from "@testing-library/react";
import {
  ChatView,
  formatSize,
  isPreviewableImage,
  type ChatViewAttachment,
  type ChatViewMessage,
} from "@nookos/ui";

// `uploadUserContent` answers an `UploadHandle` — `{ done, abort }` — so the
// composer can report progress and take an upload back (MAIN-533). These
// helpers keep each test saying what it means rather than restating the shape.
const upload = vi.hoisted(() => vi.fn());
vi.mock("@nookos/api", async (orig) => ({
  ...(await orig<typeof import("@nookos/api")>()),
  uploadUserContent: upload,
}));

const lands = (content: unknown) => ({ done: Promise.resolve(content), abort: vi.fn() });
const refused = (message: string) => ({
  done: Promise.reject(new Error(message)),
  abort: vi.fn(),
});

// Imported AFTER the mock so the hook picks it up.
const { useAttachments } = await import("./useAttachments");

afterEach(() => {
  cleanup();
  upload.mockReset();
});

const png: ChatViewAttachment = {
  id: "a1",
  filename: "shot.png",
  contentType: "image/png",
  sizeBytes: 2048,
  url: "/api/v1/user-content/c1",
};
const zip: ChatViewAttachment = {
  id: "a2",
  filename: "logs.zip",
  contentType: "application/zip",
  sizeBytes: 5 * 1024 * 1024,
  url: "/api/v1/user-content/c2",
};

function withFiles(attachments: ChatViewAttachment[], body = ""): ChatViewMessage[] {
  return [
    {
      id: "m1",
      authorId: "u1",
      authorName: "alice",
      body,
      createdAt: "2026-08-11T10:00:00Z",
      attachments,
    },
  ];
}

/** A `File` that jsdom is happy to put on a DataTransfer/clipboard. */
function file(name: string, type: string, bytes = 4): File {
  return new File([new Uint8Array(bytes)], name, { type });
}

describe("rendering a message's files (AC-4)", () => {
  it("previews an image and links it at full size", () => {
    render(<ChatView messages={withFiles([png])} onSend={vi.fn()} />);
    const img = screen.getByAltText("shot.png") as HTMLImageElement;
    expect(img.getAttribute("src")).toBe(png.url);
    // The preview IS the link to the full-size bytes — clicking it opens them.
    const link = img.closest("a") as HTMLAnchorElement;
    expect(link.getAttribute("href")).toBe(png.url);
    expect(link.getAttribute("target")).toBe("_blank");
  });

  it("chips anything else, with its type and size, downloading on click", () => {
    render(<ChatView messages={withFiles([zip])} onSend={vi.fn()} />);
    expect(screen.getByText("logs.zip")).toBeTruthy();
    expect(screen.getByText("zip · 5.0 MB")).toBeTruthy();
    const link = screen.getByText("logs.zip").closest("a") as HTMLAnchorElement;
    expect(link.getAttribute("href")).toBe(zip.url);
    expect(link.getAttribute("download")).toBe("logs.zip");
    // Never an <img>: a claimed type is enough to choose a preview and not
    // enough to be trusted with one.
    expect(screen.queryByRole("img")).toBeNull();
  });

  it("renders files on a message with no text at all (AC-2)", () => {
    render(<ChatView messages={withFiles([png])} onSend={vi.fn()} />);
    expect(screen.getByAltText("shot.png")).toBeTruthy();
  });

  it("shows nothing but the placeholder for a deleted message (AC-6)", () => {
    const [m] = withFiles([png], "gone");
    render(
      <ChatView messages={[{ ...m, deleted: true }]} onSend={vi.fn()} />,
    );
    expect(screen.getByText("message deleted")).toBeTruthy();
    expect(screen.queryByAltText("shot.png")).toBeNull();
  });
});

describe("the three ways in (AC-2)", () => {
  const staged = [
    { key: "s1", filename: "a.png", sizeBytes: 10, contentType: "image/png", contentId: "c1" },
  ];

  it("takes files from the picker", async () => {
    const onAttachFiles = vi.fn();
    render(<ChatView messages={[]} onSend={vi.fn()} onAttachFiles={onAttachFiles} />);
    const input = document.querySelector("input[type=file]") as HTMLInputElement;
    await userEvent.upload(input, file("a.png", "image/png"));
    expect(onAttachFiles).toHaveBeenCalledTimes(1);
    expect(onAttachFiles.mock.calls[0][0][0].name).toBe("a.png");
  });

  it("takes files dropped on the message list", () => {
    const onAttachFiles = vi.fn();
    render(<ChatView messages={[]} onSend={vi.fn()} onAttachFiles={onAttachFiles} />);
    const dropped = file("drag.png", "image/png");
    fireEvent.drop(screen.getByRole("log"), {
      dataTransfer: { files: [dropped], types: ["Files"] },
    });
    expect(onAttachFiles).toHaveBeenCalledWith([dropped]);
  });

  it("takes an image pasted into the composer", () => {
    const onAttachFiles = vi.fn();
    render(<ChatView messages={[]} onSend={vi.fn()} onAttachFiles={onAttachFiles} />);
    const pasted = file("screenshot.png", "image/png");
    fireEvent.paste(screen.getByLabelText("Message"), {
      clipboardData: { files: [pasted], types: ["Files"] },
    });
    expect(onAttachFiles).toHaveBeenCalledWith([pasted]);
  });

  it("sends an empty body once a file is staged, and refuses one without", async () => {
    const onSend = vi.fn();
    const { rerender } = render(
      <ChatView messages={[]} onSend={onSend} onAttachFiles={vi.fn()} staged={[]} />,
    );
    const send = screen.getByTitle("Send message") as HTMLButtonElement;
    expect(send.disabled).toBe(true);

    rerender(
      <ChatView messages={[]} onSend={onSend} onAttachFiles={vi.fn()} staged={staged} />,
    );
    expect((screen.getByTitle("Send message") as HTMLButtonElement).disabled).toBe(false);
    await userEvent.click(screen.getByTitle("Send message"));
    expect(onSend).toHaveBeenCalledWith("");
  });

  it("will not send while an upload is still in flight — drafted body included", async () => {
    const onSend = vi.fn();
    const inFlight = [{ ...staged[0], contentId: undefined, uploading: true }];
    render(
      <ChatView
        messages={[]}
        onSend={onSend}
        onAttachFiles={vi.fn()}
        staged={inFlight}
      />,
    );
    const send = () => screen.getByTitle("Send message") as HTMLButtonElement;
    expect(send().disabled).toBe(true);

    // The case the old guard could not reach: TEXT in the box satisfied the
    // first disjunct, so Send was enabled and pressing it posted the message
    // with every staged file silently dropped.
    await userEvent.type(screen.getByLabelText("Message"), "here's the log");
    expect(send().disabled).toBe(true);
    // …and Enter is the same path, so it must refuse too.
    await userEvent.type(screen.getByLabelText("Message"), "{Enter}");
    expect(onSend).not.toHaveBeenCalled();
  });

  it("sends once the last in-flight upload lands, files and all", async () => {
    const onSend = vi.fn();
    const { rerender } = render(
      <ChatView
        messages={[]}
        onSend={onSend}
        onAttachFiles={vi.fn()}
        staged={[staged[0], { key: "s2", filename: "b.zip", sizeBytes: 20, contentType: "application/zip", uploading: true }]}
      />,
    );
    await userEvent.type(screen.getByLabelText("Message"), "two files");
    // One landed, one in flight — the landed one must not be sent without the
    // other, which is what the old `every` check dropped as a set.
    expect((screen.getByTitle("Send message") as HTMLButtonElement).disabled).toBe(true);

    rerender(
      <ChatView
        messages={[]}
        onSend={onSend}
        onAttachFiles={vi.fn()}
        staged={[staged[0], { key: "s2", filename: "b.zip", sizeBytes: 20, contentType: "application/zip", contentId: "c2" }]}
      />,
    );
    await userEvent.click(screen.getByTitle("Send message"));
    expect(onSend).toHaveBeenCalledWith("two files");
  });

  it("says DM files are not encrypted, where it can be read before sending (AC-7)", () => {
    render(
      <ChatView
        messages={[]}
        onSend={vi.fn()}
        onAttachFiles={vi.fn()}
        staged={staged}
        attachNotice="Files in DMs are stored unencrypted — they are not private yet."
      />,
    );
    expect(screen.getByText(/stored unencrypted/)).toBeTruthy();
  });
});

describe("an upload that is refused (AC-8)", () => {
  it("shows the server's message and stages nothing", async () => {
    upload.mockReturnValue(refused("that file is larger than the 25 MiB upload limit"));
    const { result } = renderHook(() => useAttachments());
    await act(async () => {
      result.current.add([file("huge.bin", "application/octet-stream")]);
    });
    await waitFor(() => expect(result.current.error).toBeTruthy());
    expect(result.current.error).toContain("25 MiB upload limit");
    // Nothing left behind to send — so nothing can post a broken message.
    expect(result.current.staged).toHaveLength(0);
    // Nothing staged is `[]`, which is a real answer: post with no files.
    expect(result.current.ids()).toEqual([]);
  });

  it("answers null, not [], while an upload is in flight", async () => {
    // The two used to be the same answer, and a caller reading `[]` as "no
    // files" is what posted a message without the files it was shown.
    let land: (c: unknown) => void = () => {};
    upload.mockReturnValue({
      done: new Promise((resolve) => (land = resolve)),
      abort: vi.fn(),
    });
    const { result } = renderHook(() => useAttachments());
    await act(async () => {
      result.current.add([file("slow.bin", "application/octet-stream")]);
    });
    await waitFor(() => expect(result.current.staged).toHaveLength(1));
    expect(result.current.staged[0].uploading).toBe(true);
    expect(result.current.ids()).toBeNull();

    await act(async () => {
      land({
        id: "c-late",
        filename: "slow.bin",
        content_type: "application/octet-stream",
        size_bytes: 99,
        sha256: "abc",
        created_at: "2026-08-11T10:00:00Z",
      });
    });
    await waitFor(() => expect(result.current.ids()).toEqual(["c-late"]));
  });

  it("stages the server's record on success, ready to post", async () => {
    upload.mockReturnValue(
      lands({
        id: "c-77",
        filename: "notes.txt",
        content_type: "text/plain",
        size_bytes: 12,
        sha256: "abc",
        created_at: "2026-08-11T10:00:00Z",
      }),
    );
    const { result } = renderHook(() => useAttachments());
    await act(async () => {
      result.current.add([file("notes.txt", "text/plain")]);
    });
    await waitFor(() => expect(result.current.ids()).toEqual(["c-77"]));
    expect(result.current.staged[0].filename).toBe("notes.txt");
    expect(result.current.error).toBeNull();
  });
});

describe("the two pure decisions", () => {
  it("previews a picture and refuses a scriptable one", () => {
    expect(isPreviewableImage("image/png")).toBe(true);
    expect(isPreviewableImage("IMAGE/JPEG; charset=binary")).toBe(true);
    // An SVG is an image and can carry script — it chips.
    expect(isPreviewableImage("image/svg+xml")).toBe(false);
    expect(isPreviewableImage("application/pdf")).toBe(false);
    expect(isPreviewableImage("")).toBe(false);
  });

  it("reads sizes the way a person does", () => {
    expect(formatSize(512)).toBe("512 B");
    expect(formatSize(2048)).toBe("2.0 KB");
    expect(formatSize(5 * 1024 * 1024)).toBe("5.0 MB");
    expect(formatSize(30 * 1024 * 1024)).toBe("30 MB");
  });
});
