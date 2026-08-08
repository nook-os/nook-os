// The Builds panel's row mapping (MAIN-461 AC-2): the card key names the row,
// a keyless run (deleted card — or a private card the viewer may not see,
// whose key the listing withholds) falls back to "build", and the outcome
// occupies the meta slot when the API sends one.
import React from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { WorkspaceBuilds } from "./WorkspaceBuilds";

const get = vi.fn();
vi.mock("@nookos/api", () => ({
  api: { GET: (...args: unknown[]) => get(...args) },
}));

function mount() {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  render(
    <QueryClientProvider client={qc}>
      <WorkspaceBuilds workspaceId="ws-1" />
    </QueryClientProvider>,
  );
}

describe("WorkspaceBuilds", () => {
  beforeEach(() => {
    cleanup();
    get.mockReset();
  });

  it("names rows by card key, falls back for keyless runs, and shows the outcome", async () => {
    get.mockImplementation(async (path: string) => {
      if (path === "/api/v1/workspaces/{id}/builds") {
        return {
          data: [
            {
              id: "j1",
              state: "running",
              task_key: "MAIN-42",
              created_at: "2026-08-08T00:00:00Z",
            },
            {
              id: "j2",
              state: "completed",
              task_key: null,
              created_at: "2026-08-08T00:00:00Z",
              build_outcome: "pr_opened",
            },
          ],
        };
      }
      return { data: { transcript: [] } };
    });
    mount();
    const rows = await screen.findAllByTestId("build-run");
    expect(rows).toHaveLength(2);
    expect(rows[0].textContent).toContain("MAIN-42");
    expect(rows[1].textContent).toContain("build");
    expect(rows[1].textContent).toContain("pr_opened");
    // The transcript pane identifies itself as a build's, not a review's.
    expect(screen.getByTestId("build-transcript")).toBeTruthy();
  });

  it("explains itself when nothing has run", async () => {
    get.mockResolvedValue({ data: [] });
    mount();
    expect(
      (await screen.findByText(/No build has run/)).textContent,
    ).toContain("nook builds scale");
  });
});
