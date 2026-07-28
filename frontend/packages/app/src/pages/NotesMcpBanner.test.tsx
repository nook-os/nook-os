// MAIN-180: the Notes MCP helper banner — the absolute URL, Copy, the Settings
// link, and per-browser dismissal that survives a remount.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { NotesMcpBanner } from "./NotesMcpBanner";

function renderBanner() {
  return render(
    <MemoryRouter>
      <NotesMcpBanner />
    </MemoryRouter>,
  );
}

beforeEach(() => {
  localStorage.clear();
});
afterEach(() => cleanup());

describe("NotesMcpBanner", () => {
  it("shows the absolute /mcp URL and the required connector line (AC-2)", () => {
    renderBanner();
    // jsdom's default origin.
    expect(screen.getByText(`${window.location.origin}/mcp`)).toBeTruthy();
    expect(
      screen.getByText(
        /Add this as an MCP connector in ChatGPT or Claude, using an access token as the bearer\./,
      ),
    ).toBeTruthy();
  });

  it("links to Settings for the access token (AC-4)", () => {
    renderBanner();
    const link = screen.getByRole("link", { name: /Access tokens/i }) as HTMLAnchorElement;
    expect(link.getAttribute("href")).toBe("/settings");
  });

  it("Copy writes the absolute URL to the clipboard (AC-4)", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    renderBanner();
    await userEvent.click(screen.getByRole("button", { name: "Copy" }));
    expect(writeText).toHaveBeenCalledWith(`${window.location.origin}/mcp`);
  });

  it("dismissal persists per browser and does not reappear (AC-3)", async () => {
    const { unmount } = renderBanner();
    await userEvent.click(screen.getByRole("button", { name: "dismiss MCP banner" }));
    // Gone immediately…
    expect(screen.queryByText(`${window.location.origin}/mcp`)).toBeNull();
    // …and stays gone on a fresh mount (the "return visit").
    unmount();
    renderBanner();
    expect(screen.queryByText(`${window.location.origin}/mcp`)).toBeNull();
    expect(localStorage.getItem("nook.notesMcpBannerDismissed")).toBe("1");
  });
});
