// MAIN-591 AC-10: the card's "request changes" action — when it is offered,
// and what it sends.
//
// Both rules fail silently if they regress. Offered on a card with no open
// pull request it is a button that can only ever refuse; sending
// `request_changes` from the ordinary submit would turn every comment on a
// card with a PR into a rejection of it.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";

const OK = { ok: true, status: 200, statusText: "OK" } as unknown as Response;

const state = vi.hoisted(() => ({
  pr_url: null as string | null,
  column_id: "c-open",
  labels: [] as { id: string; name: string; color: string }[],
}));

const get = vi.hoisted(() =>
  vi.fn(async (path: string) => {
    if (path === "/api/v1/tasks/{id}") {
      return {
        data: {
          task: {
            id: "t-1",
            key: "MAIN-591",
            title: "a card in review",
            description: "## AC-1",
            type: "task",
            column_id: state.column_id,
            priority: 0,
            visibility: "team",
            labels: state.labels,
            pr_url: state.pr_url,
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
    // The collection is an ENVELOPE (MAIN-606); the catch-all array below would
    // hand the picker a page with no `rows` and crash the card it lives on.
    if (path === "/api/v1/workspaces")
      return { data: { rows: [], next_cursor: null }, response: OK };
    return { data: [], response: OK };
  }),
);

/** The server's answer to the comment POST. `undefined` data is what a 4xx
 *  looks like from `api.POST` — it does NOT throw. Typed with its arguments so
 *  the body assertion below is checked rather than cast. */
const post = vi.hoisted(() =>
  vi.fn(async (_path: string, _init: { body: unknown }) => ({
    data: undefined as unknown,
  })),
);

vi.mock("@nookos/api", () => ({
  api: { GET: get, POST: post, DELETE: vi.fn(), PUT: vi.fn(), PATCH: vi.fn() },
}));

// The composer is CodeMirror, which jsdom cannot be typed into — stubbed as a
// textarea labelled by its placeholder, exactly as TaskDetail.test.tsx does.
vi.mock("@nookos/ui", () => ({
  Pill: ({ children }: { children: React.ReactNode }) => <span>{children}</span>,
  Markdown: ({ src }: { src: string }) => <div>{src}</div>,
  MarkdownEditor: ({
    value,
    onChange,
    placeholder,
  }: {
    value: string;
    onChange: (v: string) => void;
    placeholder?: string;
  }) => (
    <textarea
      aria-label={placeholder ?? "editor"}
      value={value}
      onChange={(e) => onChange(e.target.value)}
    />
  ),
  EditableMarkdown: ({ src }: { src: string }) => <div>{src}</div>,
  Select: () => null,
  TypeSelect: () => null,
  TYPE_META: [
    { value: "task", label: "Task", Icon: () => null },
    { value: "epic", label: "Epic", Icon: () => null },
  ],
  VISIBILITY_META: [
    { value: "private", label: "Private", tooltip: "", Icon: () => null },
    { value: "team", label: "Team", tooltip: "", Icon: () => null },
    { value: "org", label: "Org", tooltip: "", Icon: () => null },
  ],
  useAnchoredMenu: () => ({
    hostRef: { current: null },
    portal: (c: React.ReactNode) => c,
  }),
  // The workspace field is a paged picker now (MAIN-606), and its search box
  // comes from here — an unnamed export in a whole-module stub is a render
  // crash, not a missing control.
  SearchInput: () => null,
}));
vi.mock("./Interactions", () => ({ TaskInteractions: () => null }));
vi.mock("./LoopPanel", () => ({ LoopPanel: () => null }));

import { TaskDetail, canRequestChanges, commentRequestBody } from "./TaskDetail";

beforeEach(() => {
  state.pr_url = null;
  state.column_id = "c-open";
  state.labels = [];
  get.mockClear();
  post.mockClear();
  post.mockResolvedValue({ data: undefined });
});
afterEach(cleanup);

const show = () => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <TaskDetail
          taskId="t-1"
          columns={[
            { id: "c-open", name: "In Review", type: "review" },
            { id: "c-done", name: "Done", type: "completed" },
          ]}
          onClose={() => {}}
        />
      </MemoryRouter>
    </QueryClientProvider>,
  );
};

const rejectButton = () => screen.queryByRole("button", { name: /request changes/i });

describe("canRequestChanges", () => {
  it("needs a recorded PR on a card whose column is not a done one", () => {
    expect(canRequestChanges("https://github.com/a/b/pull/7", "review")).toBe(true);
    expect(canRequestChanges(null, "review")).toBe(false);
    expect(canRequestChanges(undefined, "review")).toBe(false);
    // A merged or abandoned PR moves its card here, and a PR that is not open
    // is one the server refuses anyway (AC-2).
    expect(canRequestChanges("https://github.com/a/b/pull/7", "completed")).toBe(false);
    expect(canRequestChanges("https://github.com/a/b/pull/7", "canceled")).toBe(false);
  });
});

describe("commentRequestBody", () => {
  it("carries request_changes only for that submit, and independently of the unblock", () => {
    expect(commentRequestBody("just asking")).toEqual({ body_md: "just asking" });
    expect(commentRequestBody("just asking", false, false)).toEqual({
      body_md: "just asking",
    });
    expect(commentRequestBody("fix it", false, true)).toEqual({
      body_md: "fix it",
      request_changes: true,
    });
    // AC-1: both is valid and does both.
    expect(commentRequestBody("ruled, and fix it", true, true)).toEqual({
      body_md: "ruled, and fix it",
      clear_escalation: true,
      request_changes: true,
    });
  });
});

describe("the card's request-changes action", () => {
  it("is offered beside an open PR, and is dead until a ruling is typed", async () => {
    state.pr_url = "https://github.com/acme/api/pull/7";
    show();
    await screen.findByRole("button", { name: /^comment$/i });
    const button = rejectButton() as HTMLButtonElement | null;
    expect(button).not.toBeNull();
    expect(button!.disabled).toBe(true);
  });

  it("is not offered on a card with no pull request", async () => {
    show();
    await screen.findByRole("button", { name: /^comment$/i });
    expect(rejectButton()).toBeNull();
  });

  // MAIN-608 AC-3: the two conditional buttons are independent, so a card with
  // an open PR that is ALSO stopped shows both — the case a row holding one of
  // them at a time would render wrong.
  it("shares the row with 'comment and unblock' when the card is also escalated", async () => {
    state.pr_url = "https://github.com/acme/api/pull/7";
    state.labels = [{ id: "l-1", name: "needs-human-review", color: "#f00" }];
    show();
    const submit = await screen.findByRole("button", { name: /^comment$/i });
    const row = submit.parentElement as HTMLElement;
    expect([...row.querySelectorAll("button")].map((b) => b.textContent?.trim())).toEqual([
      "Attach",
      "comment and unblock",
      "request changes",
      "comment",
    ]);
  });

  it("is not offered once the card is done — its PR is merged or closed", async () => {
    state.pr_url = "https://github.com/acme/api/pull/7";
    state.column_id = "c-done";
    show();
    await screen.findByRole("button", { name: /^comment$/i });
    expect(rejectButton()).toBeNull();
  });
});

describe("a refused ruling", () => {
  const compose = async (text: string) => {
    show();
    const editor = (await screen.findByLabelText(
      "Add a comment…",
    )) as HTMLTextAreaElement;
    fireEvent.change(editor, { target: { value: text } });
    const button = (await screen.findByRole("button", {
      name: /request changes/i,
    })) as HTMLButtonElement;
    await waitFor(() => expect(button.disabled).toBe(false));
    fireEvent.click(button);
    return editor;
  };

  it("keeps the typed text in the composer — the server said no, not the user", async () => {
    state.pr_url = "https://github.com/acme/api/pull/7";
    const editor = await compose("AC-2 is not met");

    await waitFor(() => expect(post).toHaveBeenCalled());
    expect(post.mock.calls[0][1].body).toEqual({
      body_md: "AC-2 is not met",
      request_changes: true,
    });
    // `api.POST` resolves with no data on a 4xx rather than throwing, which is
    // what used to let the success path clear the box.
    await waitFor(() => expect(editor.value).toBe("AC-2 is not met"));
  });

  it("clears it once the comment is actually created", async () => {
    state.pr_url = "https://github.com/acme/api/pull/7";
    post.mockResolvedValue({ data: { id: "c-1" } });
    const editor = await compose("AC-2 is not met");
    await waitFor(() => expect(editor.value).toBe(""));
  });
});
