import { describe, expect, it } from "vitest";
import { formatSize, isImage } from "./attachments";
import { messageFrom } from "@nookos/api";

describe("formatSize", () => {
  it("reads as binary units, matching the cap the server enforces", () => {
    expect(formatSize(0)).toBe("0 B");
    expect(formatSize(900)).toBe("900 B");
    expect(formatSize(1024)).toBe("1.0 KiB");
    expect(formatSize(30 * 1024 * 1024)).toBe("30 MiB");
    expect(formatSize(-1)).toBe("—");
  });
});

describe("isImage", () => {
  it("previews the plain raster types and nothing else", () => {
    expect(isImage("image/png")).toBe(true);
    expect(isImage("IMAGE/JPEG; charset=binary")).toBe(true);
    expect(isImage("application/zip")).toBe(false);
    // An SVG is a scriptable document; the server sandboxes it, an <img> would not.
    expect(isImage("image/svg+xml")).toBe(false);
    expect(isImage("")).toBe(false);
  });
});

describe("messageFrom", () => {
  it("prefers the server's sentence over anything invented here (AC-9)", () => {
    expect(messageFrom(413, '{"error":"that file is larger than the 30 MiB upload limit"}')).toBe(
      "that file is larger than the 30 MiB upload limit",
    );
  });

  it("never shows a person a body that is not prose", () => {
    expect(messageFrom(413, "<html>413 Request Entity Too Large</html>")).toBe(
      "that file is too large to upload",
    );
    expect(messageFrom(503, "")).toBe("the file store is unavailable — try again in a moment");
    expect(messageFrom(403, '{"error":"  "}')).toBe("you are not allowed to upload here");
  });
});
