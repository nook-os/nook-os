// MAIN-584 AC-12: the composer's second submit — when it is offered, and what
// it sends.
//
// Both rules fail silently if they regress. Shown on a card nothing stopped, it
// restarts what was never stopped; sending `clear_escalation` from the ordinary
// submit would turn every question asked on a blocked card into a restart of it
// (NG-4). The render case pins the third rule — a body is required — from the
// initial state of the button, which is the state a person actually meets.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";

const OK = { ok: true, status: 200, statusText: "OK" } as unknown as Response;

const state = vi.hoisted(() => ({ labels: [] as { id: string; name: string }[] }));

const get = vi.hoisted(() =>
  vi.fn(async (path: string) => {
    if (path === "/api/v1/tasks/{id}") {
      return {
        data: {
          task: {
            id: "t-1",
            key: "MAIN-454",
            title: "a stopped card",
            description: "## AC-1",
            type: "task",
            column_id: "c-1",
            priority: 0,
            visibility: "team",
            labels: state.labels,
            created_at: "2026-08-14T00:00:00Z",
            updated_at: "2026-08-14T00:00:00Z",
          },
          comments: [],
          blocked_by: [],
          blocking: [],
          related: [],
          is_blocked: false,
          children: [],
        },
        response: OK,
      };
    }
    return { data: [], response: OK };
  }),
);

vi.mock("@nookos/api", () => ({
  api: { GET: get, POST: vi.fn(), DELETE: vi.fn(), PUT: vi.fn(), PATCH: vi.fn() },
}));

import { TaskDetail, commentRequestBody, isEscalated } from "./TaskDetail";

beforeEach(() => {
  state.labels = [];
  get.mockClear();
});
afterEach(cleanup);

const show = () => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <TaskDetail
          taskId="t-1"
          columns={[{ id: "c-1", name: "Todo", type: "unstarted" }]}
          onClose={() => {}}
        />
      </MemoryRouter>
    </QueryClientProvider>,
  );
};

const label = (name: string) => ({
  id: `l-${name}`,
  tenant_id: "t",
  name,
  color: "#fff",
  created_at: "2026-08-14T00:00:00Z",
});
const unblockButton = () => screen.queryByRole("button", { name: /comment and unblock/i });

describe("isEscalated", () => {
  it("is true for every label that STOPS a card, and only those", () => {
    for (const stop of ["blocked", "spec-blocked", "needs-human-review"]) {
      expect(isEscalated([label(stop)])).toBe(true);
    }
    expect(isEscalated([label("agent-ready"), label("frontend")])).toBe(false);
    expect(isEscalated([])).toBe(false);
    expect(isEscalated(undefined)).toBe(false);
  });
});

describe("commentRequestBody", () => {
  it("carries clear_escalation only for the unblock submit (NG-4)", () => {
    expect(commentRequestBody("just asking")).toEqual({ body_md: "just asking" });
    expect(commentRequestBody("just asking", false)).toEqual({ body_md: "just asking" });
    expect(commentRequestBody("ruled")).not.toHaveProperty("clear_escalation");
    expect(commentRequestBody("ruled", true)).toEqual({
      body_md: "ruled",
      clear_escalation: true,
    });
  });
});

describe("the composer's button row", () => {
  it("offers the unblock submit on a stopped card, disabled until there is a ruling", async () => {
    state.labels = [label("blocked")];
    show();
    await screen.findByRole("button", { name: /^comment$/i });
    const button = unblockButton() as HTMLButtonElement | null;
    expect(button).not.toBeNull();
    expect(button!.disabled).toBe(true);
  });

  it("does not offer it on a card nothing stopped", async () => {
    state.labels = [label("agent-ready")];
    show();
    await screen.findByRole("button", { name: /^comment$/i });
    expect(unblockButton()).toBeNull();
  });
});
