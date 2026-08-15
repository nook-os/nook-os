// MAIN-451: the dashboard's Queue panel.
//
// The pure helpers are unit-tested directly; the panel itself is rendered
// against a mocked client that records EVERY request, because half this card's
// contract is which query each section sends — "On deck is exactly the builder's
// pick" is a claim about parameters, not about pixels.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";

const WORKSPACES = [
  { id: "ws-1", name: "nook-os", locations: [] },
  { id: "ws-2", name: "widgets", locations: [] },
];

/** A board card, with only the fields the panel reads. */
function task(over: Record<string, unknown> = {}) {
  return {
    id: `id-${over.key ?? "MAIN-1"}`,
    key: "MAIN-1",
    title: "Do the thing",
    priority: 3,
    workspace_id: "ws-1",
    claim_expires_at: null,
    updated_at: "2026-08-07T12:00:00Z",
    ...over,
  };
}

/** Every request the panel made, as (path, query) — the assertion surface. */
const calls = vi.hoisted(() => [] as { path: string; query: Record<string, unknown> }[]);
/** What `/api/v1/tasks` answers, keyed by a signature of its query. */
const answers = vi.hoisted(() => new Map<string, unknown[]>());

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (
      path: string,
      opts?: { params?: { query?: Record<string, unknown>; path?: { id?: string } } },
    ) => {
      const query = opts?.params?.query ?? {};
      calls.push({ path, query });
      if (path === "/api/v1/workspaces")
        return { data: { rows: WORKSPACES, next_cursor: null } };
      if (path === "/api/v1/workspaces/{id}") {
        const id = opts?.params?.path?.id;
        return { data: WORKSPACES.find((w) => w.id === id) ?? null };
      }
      if (path !== "/api/v1/tasks") return { data: [] };
      return { data: answers.get(signature(query)) ?? [] };
    }),
  },
}));

/// Which SECTION a query belongs to, reduced to one string. Deliberately built
/// from the filters rather than from the react-query key, so a test that names a
/// section is asserting the request the server would actually receive.
function signature(q: Record<string, unknown>): string {
  if (q.column_type) return `column:${q.column_type}`;
  const label = Array.isArray(q.label) ? q.label[0] : q.label;
  if (label === "agent-ready") return "on-deck";
  if (label) return `label:${label}`;
  return "other";
}

import { QueuePanel, claimAgeMs, mergeById, shortAge, STALE_CLAIM_MS } from "./QueuePanel";

function mount() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <QueuePanel />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

/** The query a given section sent, or undefined if it never asked. */
const sectionQuery = (name: string) =>
  calls.find((c) => c.path === "/api/v1/tasks" && signature(c.query) === name)?.query;

beforeEach(() => {
  calls.length = 0;
  answers.clear();
  localStorage.clear();
});
afterEach(cleanup);

describe("claimAgeMs", () => {
  const now = Date.parse("2026-08-07T12:00:00Z");

  it("is null for a card nothing claimed", () => {
    // A human dragged this into In Progress. Labelling it with a claim age
    // would be inventing a claim that does not exist.
    expect(claimAgeMs(task({ claim_expires_at: null }) as never, now)).toBeNull();
  });

  it("measures from the claimed card's last activity", () => {
    const t = task({
      claim_expires_at: "2026-08-07T16:00:00Z",
      updated_at: "2026-08-07T11:30:00Z",
    });
    expect(claimAgeMs(t as never, now)).toBe(30 * 60 * 1000);
  });

  it("never goes negative on a clock the server and browser disagree about", () => {
    const t = task({
      claim_expires_at: "2026-08-07T16:00:00Z",
      updated_at: "2026-08-07T12:05:00Z",
    });
    expect(claimAgeMs(t as never, now)).toBe(0);
  });

  it("is null rather than NaN on an unparseable timestamp", () => {
    const t = task({ claim_expires_at: "2026-08-07T16:00:00Z", updated_at: "nonsense" });
    expect(claimAgeMs(t as never, now)).toBeNull();
  });
});

describe("shortAge", () => {
  it("says which unit it is at every scale", () => {
    expect(shortAge(0)).toBe("just now");
    expect(shortAge(5 * 60_000)).toBe("5m");
    expect(shortAge(60 * 60_000)).toBe("1h");
    expect(shortAge(150 * 60_000)).toBe("2h 30m");
    expect(shortAge(26 * 60 * 60_000)).toBe("1d");
  });
});

describe("mergeById", () => {
  it("keeps a card carrying both labels exactly once", () => {
    // The reason two queries exist at all: `label=` ANDs on the server, so
    // blocked and human-review-required are asked separately — and a card with
    // both would otherwise be listed twice under one heading.
    const both = task({ key: "MAIN-9", id: "dup" });
    expect(mergeById([both] as never, [both] as never).map((t) => t.id)).toEqual(["dup"]);
  });

  it("preserves order, first list first", () => {
    const a = task({ key: "A", id: "a" });
    const b = task({ key: "B", id: "b" });
    expect(mergeById([a] as never, [b] as never).map((t) => t.id)).toEqual(["a", "b"]);
  });

  it("tolerates a section that has not answered yet", () => {
    expect(mergeById(undefined, undefined)).toEqual([]);
  });
});

describe("<QueuePanel> — the queries each section sends", () => {
  it("asks for exactly the builder's pick on On deck (AC-2)", async () => {
    mount();
    await waitFor(() => expect(sectionQuery("on-deck")).toBeTruthy());
    const q = sectionQuery("on-deck")!;
    // The same four filters `nook tasks --label agent-ready --not-label blocked
    // --assignee none --unblocked` sends. Anything more or less and the panel
    // stops being a preview of what the next agent takes.
    expect(q.label).toEqual(["agent-ready"]);
    expect(q.not_label).toEqual(["blocked"]);
    expect(q.assignee).toBe("none");
    expect(q.is_blocked).toBe(false);
  });

  it("asks for the other three sections by column type and label (AC-3)", async () => {
    mount();
    await waitFor(() => expect(calls.filter((c) => c.path === "/api/v1/tasks")).toHaveLength(5));
    expect(sectionQuery("column:started")).toBeTruthy();
    expect(sectionQuery("column:review")).toBeTruthy();
    // Two queries, not one with two labels — the server ANDs repeated labels.
    expect(sectionQuery("label:blocked")).toBeTruthy();
    expect(sectionQuery("label:human-review-required")).toBeTruthy();
  });

  it("sends no workspace parameter until one is chosen (AC-6 default)", async () => {
    mount();
    await waitFor(() => expect(sectionQuery("on-deck")).toBeTruthy());
    for (const c of calls.filter((x) => x.path === "/api/v1/tasks")) {
      expect(c.query.workspace).toBeUndefined();
    }
  });
});

describe("<QueuePanel> — rendering", () => {
  it("numbers On deck so the pick order is unambiguous (AC-2)", async () => {
    answers.set("on-deck", [
      task({ key: "MAIN-10", id: "a", title: "First up", priority: 1 }),
      task({ key: "MAIN-11", id: "b", title: "Then this" }),
    ]);
    mount();
    expect(await screen.findByText("MAIN-10")).toBeTruthy();
    expect(screen.getByText("01")).toBeTruthy();
    expect(screen.getByText("02")).toBeTruthy();
  });

  it("shows the key, title, priority and workspace, linking to the Board (AC-4)", async () => {
    answers.set("on-deck", [task({ key: "MAIN-10", id: "a", title: "First up", priority: 1 })]);
    mount();
    const key = await screen.findByText("MAIN-10");
    expect(key.closest("a")?.getAttribute("href")).toBe("/board?task=MAIN-10");
    expect(screen.getByText("First up")).toBeTruthy();
    expect(screen.getByTitle("priority: urgent")).toBeTruthy();
    expect(screen.getByText("nook-os")).toBeTruthy();
  });

  it("keeps a blocked card out of On deck and shows it under Blocked", async () => {
    // Two different sections, from two different queries: the panel never
    // filters client-side, so this is really asserting that the `not_label`
    // above reaches the server.
    answers.set("on-deck", []);
    answers.set("label:blocked", [task({ key: "MAIN-77", id: "blocked-1" })]);
    mount();
    expect(await screen.findByText("MAIN-77")).toBeTruthy();
    expect(
      screen.getByText("Nothing approved and waiting — no agent-ready work in the queue."),
    ).toBeTruthy();
  });

  it("marks a claim older than two hours and leaves a fresh one alone (AC-5)", async () => {
    const now = Date.now();
    answers.set("column:started", [
      task({
        key: "MAIN-20",
        id: "stale",
        title: "Abandoned",
        claim_expires_at: "2099-01-01T00:00:00Z",
        updated_at: new Date(now - STALE_CLAIM_MS - 60_000).toISOString(),
      }),
      task({
        key: "MAIN-21",
        id: "fresh",
        title: "Being worked",
        claim_expires_at: "2099-01-01T00:00:00Z",
        updated_at: new Date(now - 5 * 60_000).toISOString(),
      }),
    ]);
    mount();
    await screen.findByText("MAIN-20");

    const stale = screen.getByTitle(
      "Claimed and untouched for over two hours — the worker may be gone.",
    );
    expect(stale.className).toContain("err");
    const fresh = screen.getByTitle("Time since this claimed card was last touched.");
    expect(fresh.className).not.toContain("err");
    expect(fresh.textContent).toBe("5m");
  });

  it("shows no age at all for a card nobody claimed", async () => {
    answers.set("column:started", [
      task({ key: "MAIN-22", id: "manual", claim_expires_at: null }),
    ]);
    mount();
    await screen.findByText("MAIN-22");
    expect(screen.queryByTitle("Time since this claimed card was last touched.")).toBeNull();
  });

  it("caps On deck at ten and offers the rest on the Board (AC-2)", async () => {
    answers.set(
      "on-deck",
      Array.from({ length: 13 }, (_, i) => task({ key: `MAIN-${i}`, id: `t${i}` })),
    );
    mount();
    const more = await screen.findByText("+3 more");
    expect(more.closest("a")?.getAttribute("href")).toBe("/board");
    expect(screen.queryByText("MAIN-10")).toBeNull();
  });

  it("caps the other sections at five (AC-3)", async () => {
    answers.set(
      "column:review",
      Array.from({ length: 8 }, (_, i) => task({ key: `REV-${i}`, id: `r${i}` })),
    );
    mount();
    expect(await screen.findByText("+3 more")).toBeTruthy();
    expect(screen.queryByText("REV-5")).toBeNull();
  });

  it("gives every empty section its own message (AC-8)", async () => {
    mount();
    // Four different sentences, because the sections mean four different
    // things — and an empty Blocked is good news, not a failed load.
    expect(
      await screen.findByText("Nothing approved and waiting — no agent-ready work in the queue."),
    ).toBeTruthy();
    expect(screen.getByText("Nobody is working anything right now.")).toBeTruthy();
    expect(screen.getByText("No PRs waiting on a review.")).toBeTruthy();
    expect(screen.getByText("Nothing is waiting on a human.")).toBeTruthy();
  });
});

describe("<QueuePanel> — the workspace filter (AC-6)", () => {
  it("narrows every section, not just On deck", async () => {
    const user = userEvent.setup();
    mount();
    await waitFor(() => expect(sectionQuery("on-deck")).toBeTruthy());
    calls.length = 0;

    await user.click(screen.getByLabelText("workspace filter"));
    await user.click(await screen.findByText("widgets"));

    await waitFor(() => {
      const asked = calls.filter((c) => c.path === "/api/v1/tasks");
      expect(asked.length).toBeGreaterThanOrEqual(5);
      // EVERY one of them, which is the assertion — a filter that narrowed only
      // the section somebody was looking at would be worse than none.
      for (const c of asked) expect(c.query.workspace).toBe("ws-2");
    });
  });

  it("remembers the choice across a remount", async () => {
    const user = userEvent.setup();
    mount();
    await user.click(await screen.findByLabelText("workspace filter"));
    await user.click(await screen.findByText("widgets"));
    await waitFor(() => expect(localStorage.getItem("nook.queue.workspace")).toBe("ws-2"));

    cleanup();
    calls.length = 0;
    mount();
    await waitFor(() => expect(sectionQuery("on-deck")?.workspace).toBe("ws-2"));
  });

  it("scopes the +N more link to the chosen workspace", async () => {
    const user = userEvent.setup();
    answers.set(
      "on-deck",
      Array.from({ length: 12 }, (_, i) => task({ key: `MAIN-${i}`, id: `t${i}` })),
    );
    mount();
    await screen.findByText("+2 more");
    await user.click(screen.getByLabelText("workspace filter"));
    await user.click(await screen.findByText("widgets"));
    await waitFor(() =>
      expect(screen.getByText("+2 more").closest("a")?.getAttribute("href")).toBe(
        "/board?workspace=ws-2",
      ),
    );
  });
});

describe("<QueuePanel> — liveness (AC-7)", () => {
  it("keys every section under the prefix live.ts invalidates", async () => {
    // `task_changed` invalidates `["tasks"]`; a key that did not start there
    // would leave the panel showing work that was taken seconds ago.
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <MemoryRouter>
          <QueuePanel />
        </MemoryRouter>
      </QueryClientProvider>,
    );
    await waitFor(() => expect(sectionQuery("on-deck")).toBeTruthy());
    const keys = qc
      .getQueryCache()
      .getAll()
      .map((q) => q.queryKey as unknown[])
      .filter((k) => k[1] === "queue");
    expect(keys.length).toBe(5);
    for (const k of keys) expect(k[0]).toBe("tasks");
  });

  it("sets no refetch interval — the socket is the update mechanism", async () => {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <MemoryRouter>
          <QueuePanel />
        </MemoryRouter>
      </QueryClientProvider>,
    );
    await waitFor(() => expect(sectionQuery("on-deck")).toBeTruthy());
    for (const q of qc.getQueryCache().getAll()) {
      expect((q.options as { refetchInterval?: unknown }).refetchInterval).toBeUndefined();
    }
  });
});
