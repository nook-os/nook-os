// MAIN-209: the board opens the ticket modal by KEY (`?task=MAIN-42`), but the
// jobs API is UUID-keyed — so TaskDetail must hand LoopPanel the *resolved*
// `task.id`, not the raw `taskId` prop, or the panel 400s its list and 422s
// create. This pins that handoff. jsdom only, heavy deps mocked.
import React from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, waitFor } from "@testing-library/react";
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

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string) => {
      if (path === "/api/v1/tasks/{id}") return { data: DETAIL };
      if (path === "/api/v1/labels") return { data: [] };
      if (path === "/api/v1/workspaces") return { data: [] };
      return { data: null };
    }),
    PATCH: vi.fn(async () => ({ data: DETAIL.task })),
    POST: vi.fn(async () => ({ data: {} })),
    DELETE: vi.fn(async () => ({ data: {} })),
    PUT: vi.fn(async () => ({ data: {} })),
  },
}));

vi.mock("@nookos/ui", () => ({
  Pill: ({ children }: { children: React.ReactNode }) => <span>{children}</span>,
  Markdown: ({ src }: { src: string }) => <div>{src}</div>,
  MarkdownEditor: () => null,
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
