// MAIN-94: the channel-management modal — create (with empty-name guard),
// rename, archive, unarchive, and the archived listing — against a mocked chat
// client. jsdom only.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

// Hoisted so the vi.mock factories (also hoisted) can reference them.
const m = vi.hoisted(() => ({
  createChannel: vi.fn(async (name: string) => ({
    id: "new",
    name,
    slug: name,
    archived: false,
    created_at: "2026-07-25T12:00:00Z",
  })),
  updateChannel: vi.fn(async (id: string, patch: Record<string, unknown>) => ({
    id,
    name: "x",
    slug: "x",
    archived: patch.archived ?? false,
    created_at: "2026-07-25T12:00:00Z",
  })),
  listChannels: vi.fn(async (includeArchived?: boolean) => [
    { id: "c1", name: "general", slug: "general", archived: false, category_id: null, position: 0, created_at: "2026-07-25T09:00:00Z" },
    ...(includeArchived
      ? [{ id: "c2", name: "old-stuff", slug: "old-stuff", archived: true, category_id: null, position: 0, created_at: "2026-07-25T08:00:00Z" }]
      : []),
  ]),
  // Categories (MAIN-179).
  listCategories: vi.fn(async () => [
    { id: "cat1", name: "Team", owner_type: "tenant", position: 0, created_at: "2026-07-25T09:00:00Z" },
  ]),
  createCategory: vi.fn(async () => ({})),
  renameCategory: vi.fn(async () => ({})),
  deleteCategory: vi.fn(async () => undefined),
  placeChannel: vi.fn(async () => undefined),
  askText: vi.fn(),
  askConfirm: vi.fn(async (_opts: { title: string; description: string }) => true),
}));
const { createChannel, updateChannel, listChannels, askText } = m;

vi.mock("@nookos/api", () => ({
  createChannel: m.createChannel,
  updateChannel: m.updateChannel,
  listChannels: m.listChannels,
  listCategories: m.listCategories,
  createCategory: m.createCategory,
  renameCategory: m.renameCategory,
  deleteCategory: m.deleteCategory,
  placeChannel: m.placeChannel,
}));
vi.mock("../dialogs", () => ({ askText: m.askText, askConfirm: m.askConfirm, notify: vi.fn() }));

import { ChannelManager } from "./ChannelManager";

function renderModal() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <ChannelManager onClose={() => {}} />
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  createChannel.mockClear();
  updateChannel.mockClear();
  askText.mockReset();
  m.createCategory.mockClear();
  m.deleteCategory.mockClear();
  m.askConfirm.mockClear();
});
afterEach(() => cleanup());

describe("ChannelManager (MAIN-94)", () => {
  it("lists active channels and the archived view (AC-1/AC-6)", async () => {
    renderModal();
    expect(await screen.findByText("general")).toBeTruthy();
    // The modal requests archived-inclusive, so the archived one shows up.
    expect(await screen.findByText("old-stuff")).toBeTruthy();
    expect(screen.getByText("Archived")).toBeTruthy();
    expect(listChannels).toHaveBeenCalledWith(true);
  });

  it("creates a channel from a non-empty name (AC-2), defaulting to team owner", async () => {
    renderModal();
    await screen.findByText("general");
    await userEvent.type(screen.getByLabelText("new channel name"), "release-notes");
    await userEvent.click(screen.getByRole("button", { name: /create/i }));
    // The owner selector defaults to "tenant" (My team) — MAIN-112.
    expect(createChannel).toHaveBeenCalledWith("release-notes", "tenant");
  });

  it("creates an org channel when the owner selector is set to My org (MAIN-112 AC-4)", async () => {
    renderModal();
    await screen.findByText("general");
    await userEvent.type(screen.getByLabelText("new channel name"), "org-wide");
    await userEvent.selectOptions(screen.getByLabelText("channel owner"), "org");
    await userEvent.click(screen.getByRole("button", { name: /create/i }));
    expect(createChannel).toHaveBeenCalledWith("org-wide", "org");
  });

  it("rejects an empty name inline and creates nothing (AC-2)", async () => {
    renderModal();
    await screen.findByText("general");
    await userEvent.click(screen.getByRole("button", { name: /create/i }));
    expect(await screen.findByText("A channel needs a name.")).toBeTruthy();
    expect(createChannel).not.toHaveBeenCalled();
  });

  it("renames via the text dialog (AC-3)", async () => {
    askText.mockResolvedValueOnce("releases");
    renderModal();
    await screen.findByText("general");
    await userEvent.click(screen.getByTitle("rename"));
    await waitFor(() =>
      expect(updateChannel).toHaveBeenCalledWith("c1", { name: "releases" }),
    );
  });

  it("archives an active channel (AC-4)", async () => {
    renderModal();
    await screen.findByText("general");
    await userEvent.click(screen.getByTitle("archive"));
    expect(updateChannel).toHaveBeenCalledWith("c1", { archived: true });
  });

  it("unarchives from the archived view (AC-4)", async () => {
    renderModal();
    await screen.findByText("old-stuff");
    await userEvent.click(screen.getByTitle("unarchive"));
    expect(updateChannel).toHaveBeenCalledWith("c2", { archived: false });
  });

  // MAIN-179 AC-3: category create + delete-with-warning.
  it("creates a category from the modal", async () => {
    renderModal();
    await userEvent.type(await screen.findByLabelText("new category name"), "Design");
    await userEvent.click(screen.getByText("add category"));
    await waitFor(() => expect(m.createCategory).toHaveBeenCalledWith("Design"));
  });

  it("warns that a deleted category's channels become uncategorized", async () => {
    renderModal();
    // The existing "Team" category renders with a delete control (its name also
    // appears as a channel's category option, so key on the unique button).
    await userEvent.click(await screen.findByTitle("delete category"));
    await waitFor(() => expect(m.askConfirm).toHaveBeenCalled());
    expect(m.askConfirm.mock.calls[0][0].description).toMatch(/uncategorized/i);
    await waitFor(() => expect(m.deleteCategory).toHaveBeenCalledWith("cat1"));
  });
});
