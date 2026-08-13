// Inbound forge webhooks (MAIN-554 AC-8): the URL to paste, the events to
// subscribe, and a secret shown exactly once.
import React from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";

const DELIVERY_URL = "https://nook.example/api/v1/hooks/github/ws-1";

const state = vi.hoisted(() => ({ set: false }));
const put = vi.hoisted(() =>
  vi.fn(async () => ({ data: { secret: "sup3rs3cret", delivery_url: DELIVERY_URL } })),
);
const del = vi.hoisted(() => vi.fn(async () => ({ data: { set: false, delivery_url: DELIVERY_URL } })));
const confirmed = vi.hoisted(() => ({ answer: true }));

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string) => {
      if (path.includes("webhook-secret"))
        return { data: { set: state.set, delivery_url: DELIVERY_URL } };
      return { data: [] };
    }),
    PUT: put,
    DELETE: del,
  },
}));

vi.mock("../dialogs", () => ({
  askConfirm: vi.fn(async () => confirmed.answer),
  notify: vi.fn(async () => {}),
  askChoice: vi.fn(),
  askForm: vi.fn(),
  askText: vi.fn(),
}));

import { WorkspaceWebhooks } from "./Workspaces";

beforeEach(() => {
  cleanup();
  state.set = false;
  confirmed.answer = true;
  put.mockClear();
  del.mockClear();
});

function renderIt() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <MemoryRouter>
      <QueryClientProvider client={qc}>
        <WorkspaceWebhooks workspaceId="ws-1" />
      </QueryClientProvider>
    </MemoryRouter>,
  );
}

describe("WorkspaceWebhooks", () => {
  it("shows the exact delivery URL and the events to subscribe", async () => {
    renderIt();
    // The URL is the server's, never assembled here: only the control plane
    // knows this deployment's public base. Found BY the URL rather than by the
    // test id, so the placeholder the panel shows before the query settles
    // cannot satisfy this.
    const url = await screen.findByText(DELIVERY_URL);
    expect(url.dataset.testid).toBe("webhook-delivery-url");
    const panel = (await screen.findByText(/Add webhook/)).parentElement!;
    for (const event of [
      "pull_request",
      "check_suite",
      "pull_request_review",
      "issue_comment",
    ]) {
      expect(panel.textContent).toContain(event);
    }
  });

  it("says whether a secret is currently set", async () => {
    renderIt();
    expect(await screen.findByText(/not configured/)).toBeTruthy();
    cleanup();
    state.set = true;
    renderIt();
    expect(await screen.findByText(/secret set/)).toBeTruthy();
  });

  it("surfaces a generated secret once, warned and copyable", async () => {
    renderIt();
    await userEvent.click(await screen.findByRole("button", { name: /generate secret/i }));
    const once = await screen.findByTestId("webhook-secret-once");
    expect(once.textContent).toContain("sup3rs3cret");
    // The warning is the whole affordance: there is no read path that
    // reproduces this value, so a person who closes the panel rotates.
    expect(once.textContent).toMatch(/shown only once/i);
    expect(once.textContent).toMatch(/cannot be read back/i);
    expect(screen.getByRole("button", { name: /copy secret/i })).toBeTruthy();

    // Dismissing it takes the value off screen and does not put it back.
    await userEvent.click(screen.getByRole("button", { name: /^done$/i }));
    expect(screen.queryByTestId("webhook-secret-once")).toBeNull();
  });

  it("asks before rotating, because the old secret stops working at once", async () => {
    state.set = true;
    confirmed.answer = false;
    renderIt();
    await userEvent.click(await screen.findByRole("button", { name: /rotate secret/i }));
    expect(put).not.toHaveBeenCalled();
    expect(screen.queryByTestId("webhook-secret-once")).toBeNull();
  });

  it("clears the secret on confirmation", async () => {
    state.set = true;
    renderIt();
    await userEvent.click(await screen.findByRole("button", { name: /^clear$/i }));
    expect(del).toHaveBeenCalledWith(
      "/api/v1/workspaces/{id}/webhook-secret",
      expect.objectContaining({ params: { path: { id: "ws-1" } } }),
    );
  });
});
