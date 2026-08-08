// The workspace's own forge token (MAIN-456): write-only, state-only reads.
import React from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";

const state = vi.hoisted(() => ({ set: false }));
const put = vi.hoisted(() => vi.fn(async () => ({ data: { set: true } })));

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string) => {
      if (path.includes("gh-token")) return { data: { set: state.set } };
      return { data: [] };
    }),
    PUT: put,
  },
}));

import { WorkspaceForgeToken } from "./Workspaces";

beforeEach(() => {
  cleanup();
  state.set = false;
  put.mockClear();
});

function renderIt() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <MemoryRouter>
      <QueryClientProvider client={qc}>
        <WorkspaceForgeToken workspaceId="ws-1" />
      </QueryClientProvider>
    </MemoryRouter>,
  );
}

describe("WorkspaceForgeToken", () => {
  it("says the fleet fallback applies when no token is set", async () => {
    renderIt();
    expect((await screen.findByText(/fleet fallback applies/)).textContent).toBeTruthy();
  });

  it("sends the token and clears the box — the value is never re-shown", async () => {
    renderIt();
    const input = (await screen.findByPlaceholderText(/github_pat_/)) as HTMLInputElement;
    // A password input, so a shoulder-surfer sees dots even while typing.
    expect(input.type).toBe("password");
    await userEvent.type(input, "gho_abc");
    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));
    expect(put).toHaveBeenCalledWith(
      "/api/v1/workspaces/{id}/gh-token",
      expect.objectContaining({ body: { token: "gho_abc" } }),
    );
    expect(input.value).toBe("");
  });

  it("offers clear only when a token exists, and clears with null", async () => {
    state.set = true;
    renderIt();
    await screen.findByText("set");
    await userEvent.click(screen.getByRole("button", { name: /^clear$/i }));
    expect(put).toHaveBeenCalledWith(
      "/api/v1/workspaces/{id}/gh-token",
      expect.objectContaining({ body: { token: null } }),
    );
  });
});
