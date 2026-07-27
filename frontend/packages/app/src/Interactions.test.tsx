// MAIN-159 web surface: the top-bar pending-interactions indicator wired to a
// mocked control-plane client. It proves the badge shows the pending count,
// opening the panel renders a prompt, and both a structured choice and a
// free-text Send POST to the answer endpoint for the right interaction. jsdom
// only, no control plane.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

const PENDING = [
  {
    id: "ixn-1",
    tenant_id: "t",
    task_id: "task-1",
    prompt: "Which branch should I base this on?",
    choices: ["main", "develop"],
    state: "pending",
    created_at: "2026-07-27T10:00:00Z",
    updated_at: "2026-07-27T10:00:00Z",
  },
  {
    id: "ixn-2",
    tenant_id: "t",
    prompt: "Proceed with the migration?",
    state: "pending",
    created_at: "2026-07-27T10:01:00Z",
    updated_at: "2026-07-27T10:01:00Z",
  },
];

const post = vi.hoisted(() => vi.fn(async () => ({ data: {} })));

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async () => ({ data: PENDING })),
    POST: post,
  },
}));

import { PendingInteractions } from "./Interactions";

function renderIndicator() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <PendingInteractions />
    </QueryClientProvider>,
  );
}

beforeEach(() => post.mockClear());
afterEach(() => cleanup());

describe("PendingInteractions", () => {
  it("badges the pending count and opens a panel with a prompt", async () => {
    renderIndicator();
    const trigger = await screen.findByLabelText("2 pending interactions");
    expect(trigger.textContent).toContain("2");
    await userEvent.click(trigger);
    expect(
      await screen.findByText("Which branch should I base this on?"),
    ).toBeTruthy();
  });

  it("answers with a structured choice via the answer endpoint", async () => {
    renderIndicator();
    await userEvent.click(await screen.findByLabelText("2 pending interactions"));
    await userEvent.click(await screen.findByText("develop"));
    await waitFor(() => expect(post).toHaveBeenCalled());
    expect(post).toHaveBeenCalledWith("/api/v1/interactions/{id}/answer", {
      params: { path: { id: "ixn-1" } },
      body: { response: "develop" },
    });
  });

  it("answers with free text and Send", async () => {
    renderIndicator();
    await userEvent.click(await screen.findByLabelText("2 pending interactions"));
    await screen.findByText("Proceed with the migration?");
    const inputs = screen.getAllByLabelText("reply");
    await userEvent.type(inputs[1], "yes, go");
    await userEvent.click(screen.getAllByLabelText("send reply")[1]);
    await waitFor(() => expect(post).toHaveBeenCalled());
    expect(post).toHaveBeenCalledWith("/api/v1/interactions/{id}/answer", {
      params: { path: { id: "ixn-2" } },
      body: { response: "yes, go" },
    });
  });
});
