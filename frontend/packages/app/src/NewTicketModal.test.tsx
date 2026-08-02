// The board's idea → ticket path (MAIN-364). The behaviours worth pinning are
// the ones a PM would notice going wrong: it lands in TRIAGE, it carries the
// workspace, and the AI mode starts the right loop for the type.
import React from "react";
import { describe, expect, it, vi, beforeEach } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { NewTicketModal } from "./NewTicketModal";

const posts: { path: string; body: Record<string, unknown> }[] = [];

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async () => ({
      data: [
        { id: "ws-1", name: "acme/services" },
        { id: "ws-2", name: "acme/widgets" },
      ],
    })),
    POST: vi.fn(async (path: string, opts: { body: Record<string, unknown> }) => {
      posts.push({ path, body: opts.body });
      return { data: { id: "task-1" } };
    }),
  },
}));

function wrap(ui: React.ReactElement) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(<QueryClientProvider client={qc}>{ui}</QueryClientProvider>);
}

const onCreated = vi.fn();

beforeEach(() => {
  cleanup();
  posts.length = 0;
  onCreated.mockReset();
});

describe("filing work from the board", () => {
  it("files into triage and starts a spec draft from what was typed", async () => {
    wrap(<NewTicketModal boardId="b1" onClose={() => {}} onCreated={onCreated} />);

    await userEvent.type(
      screen.getByPlaceholderText(/describe the idea/i),
      "Add a dark mode toggle",
    );
    await userEvent.click(screen.getByRole("button", { name: /draft it/i }));

    await waitFor(() => expect(posts).toHaveLength(2));

    const task = posts[0];
    expect(task.path).toBe("/api/v1/boards/{id}/tasks");
    expect(task.body.title).toBe("Add a dark mode toggle");
    expect(task.body.column_type).toBe("backlog");

    // The seed is the human's own words, and the kind matches the type.
    expect(posts[1].path).toBe("/api/v1/jobs");
    expect(posts[1].body).toMatchObject({
      kind: "spec",
      target_task_id: "task-1",
      seed: "Add a dark mode toggle",
    });
    expect(onCreated).toHaveBeenCalledWith({ taskId: "task-1", drafting: true });
  });

  it("an epic runs the decomposer instead of the spec interview", async () => {
    wrap(
      <NewTicketModal
        boardId="b1"
        initialType="epic"
        onClose={() => {}}
        onCreated={onCreated}
      />,
    );

    await userEvent.type(screen.getByPlaceholderText(/describe the idea/i), "Billing");
    await userEvent.click(screen.getByRole("button", { name: /draft it/i }));

    await waitFor(() => expect(posts).toHaveLength(2));
    expect(posts[0].body.type).toBe("epic");
    expect(posts[1].body).toMatchObject({ kind: "decompose" });
  });

  it("files without a loop when the human wants to write it themselves", async () => {
    wrap(<NewTicketModal boardId="b1" onClose={() => {}} onCreated={onCreated} />);

    await userEvent.type(screen.getByPlaceholderText(/describe the idea/i), "Tidy the logs");
    await userEvent.click(screen.getByRole("radio", { name: /file it myself/i }));
    await userEvent.click(screen.getByRole("button", { name: /file it/i }));

    await waitFor(() => expect(onCreated).toHaveBeenCalled());
    // Exactly one call: no job was started.
    expect(posts).toHaveLength(1);
    expect(onCreated).toHaveBeenCalledWith({ taskId: "task-1", drafting: false });
  });

  it("carries the chosen workspace, because that is what routes the work", async () => {
    wrap(<NewTicketModal boardId="b1" onClose={() => {}} onCreated={onCreated} />);

    await userEvent.type(screen.getByPlaceholderText(/describe the idea/i), "Fix the widget");
    // `Select` is a custom listbox (a native <select> cannot render an icon),
    // so this is click-the-trigger, click-the-option.
    await userEvent.click(await screen.findByRole("button", { name: /workspace/i }));
    await userEvent.click(await screen.findByRole("option", { name: /acme\/widgets/i }));
    await userEvent.click(screen.getByRole("button", { name: /draft it/i }));

    await waitFor(() => expect(posts.length).toBeGreaterThan(0));
    expect(posts[0].body.workspace_id).toBe("ws-2");
  });

  it("keeps a multi-line idea's first line as the title and the rest as body", async () => {
    wrap(<NewTicketModal boardId="b1" onClose={() => {}} onCreated={onCreated} />);

    const box = screen.getByPlaceholderText(/describe the idea/i);
    await userEvent.type(box, "Short title{Enter}the long explanation");
    await userEvent.click(screen.getByRole("button", { name: /draft it/i }));

    await waitFor(() => expect(posts.length).toBeGreaterThan(0));
    expect(posts[0].body.title).toBe("Short title");
    expect(posts[0].body.description).toBe("the long explanation");
  });
});
