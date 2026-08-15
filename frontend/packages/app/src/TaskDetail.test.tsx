// MAIN-209: the board opens the ticket modal by KEY (`?task=MAIN-42`), but the
// jobs API is UUID-keyed — so TaskDetail must hand LoopPanel the *resolved*
// `task.id`, not the raw `taskId` prop, or the panel 400s its list and 422s
// create. This pins that handoff. jsdom only, heavy deps mocked.
import React from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

// Capture the taskId LoopPanel is rendered with.
const captured = vi.hoisted(() => ({ loopTaskId: undefined as string | undefined }));

// The task-detail response: opened by KEY, but its real id is a UUID.
const DETAIL = {
  task: {
    id: "uuid-real-1",
    key: "MAIN-42",
    title: "A ticket",
    type: "task",
    description: "body",
    priority: 2,
    visibility: "team",
    workspace_id: null,
    column_id: "col1",
    created_by: "u1",
    assignee_user_id: null,
    labels: [],
    branch: null,
    pr_url: null,
    url: "http://x/board?task=MAIN-42",
    archived_at: null,
    parent_task_id: null,
    updated_at: "2026-07-25T00:00:00Z",
  },
  comments: [],
  blocked_by: [],
  blocking: [],
  related: [],
  is_blocked: false,
  children: [],
};

// The upload half is stubbed alongside the API client, because TaskDetail
// reaches both through this one module (MAIN-533).
const uploads = vi.hoisted(() => ({
  reject: undefined as ((e: Error) => void) | undefined,
}));

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string) => {
      if (path === "/api/v1/tasks/{id}") return { data: DETAIL };
      if (path === "/api/v1/labels") return { data: [] };
      if (path === "/api/v1/workspaces") return { data: { rows: [], next_cursor: null } };
      if (path === "/api/v1/tasks/{id}/attachments") return { data: [] };
      return { data: null };
    }),
    PATCH: vi.fn(async () => ({ data: DETAIL.task })),
    POST: vi.fn(async () => ({ data: {} })),
    DELETE: vi.fn(async () => ({ data: {} })),
    PUT: vi.fn(async () => ({ data: {} })),
  },
  contentNeedsFetch: () => false,
  userContentUrl: (id: string) => `/api/v1/user-content/${id}`,
  userContentObjectUrl: async (id: string) => `blob:${id}`,
  messageFrom: () => "the upload failed",
  uploadUserContent: () => ({
    done: new Promise((_res, rej) => {
      uploads.reject = rej as (e: Error) => void;
    }),
    abort: () => {},
  }),
}));

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
  // The type control moved to @nookos/ui (MAIN-174); this file stubs the whole
  // module, so a new export has to be named here or TaskDetail cannot render.
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
  useAnchoredMenu: () => ({ hostRef: { current: null }, portal: (c: React.ReactNode) => c }),
  // The workspace field is a paged picker now (MAIN-606), and its search box
  // comes from here — an unnamed export in a whole-module stub is a render
  // crash, not a missing control.
  SearchInput: () => null,
}));

vi.mock("./Interactions", () => ({ TaskInteractions: () => null }));
vi.mock("./LoopPanel", () => ({
  LoopPanel: (props: { taskId: string }) => {
    captured.loopTaskId = props.taskId;
    return null;
  },
}));

import { TaskDetail } from "./TaskDetail";

afterEach(() => {
  cleanup();
  captured.loopTaskId = undefined;
});

describe("TaskDetail → LoopPanel (MAIN-209)", () => {
  it("passes the fetched task.id (UUID), not the URL key, to LoopPanel", async () => {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <MemoryRouter>
        <QueryClientProvider client={qc}>
          <TaskDetail
            taskId="MAIN-42"
            columns={[{ id: "col1", name: "Todo" }]}
            onClose={() => {}}
          />
        </QueryClientProvider>
      </MemoryRouter>,
    );
    // Opened by key, but LoopPanel receives the resolved UUID.
    await waitFor(() => expect(captured.loopTaskId).toBe("uuid-real-1"));
    expect(captured.loopTaskId).not.toBe("MAIN-42");
  });
});

describe("TaskDetail → attachments (MAIN-533)", () => {
  it("a failed paste-upload leaves the comment being written intact (AC-5)", async () => {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <MemoryRouter>
        <QueryClientProvider client={qc}>
          <TaskDetail
            taskId="MAIN-42"
            columns={[{ id: "col1", name: "Todo" }]}
            onClose={() => {}}
          />
        </QueryClientProvider>
      </MemoryRouter>,
    );

    const composer = (await screen.findByLabelText("Add a comment…")) as HTMLTextAreaElement;
    fireEvent.change(composer, { target: { value: "half a thought" } });

    const png = new File(["bytes"], "shot.png", { type: "image/png" });
    fireEvent.paste(composer, {
      clipboardData: { items: [{ kind: "file", getAsFile: () => png }] },
    });

    await waitFor(() => expect(uploads.reject).toBeTruthy());
    uploads.reject!(new Error("the file store is unavailable"));

    await screen.findByText("the file store is unavailable");
    expect((screen.getByLabelText("Add a comment…") as HTMLTextAreaElement).value).toBe(
      "half a thought",
    );
  });
});
