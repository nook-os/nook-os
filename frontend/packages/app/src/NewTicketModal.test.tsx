// The board's prompt → agent path (MAIN-364). The behaviours worth pinning are
// the ones a PM would notice going wrong: the prompt reaches the agent WHOLE,
// it lands in triage, it carries the workspace, and an epic runs the
// decomposer.
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

const box = () => screen.getByPlaceholderText(/describe what you want/i);

describe("starting a spec loop from the board", () => {
  it("files into triage and hands the agent the prompt", async () => {
    wrap(<NewTicketModal boardId="b1" onClose={() => {}} onCreated={onCreated} />);

    await userEvent.type(box(), "Add a dark mode toggle");
    await userEvent.click(screen.getByRole("button", { name: /start drafting/i }));

    await waitFor(() => expect(posts).toHaveLength(2));
    expect(posts[0].path).toBe("/api/v1/boards/{id}/tasks");
    expect(posts[0].body.column_type).toBe("backlog");
    expect(posts[1].body).toMatchObject({
      kind: "spec",
      target_task_id: "task-1",
      seed: "Add a dark mode toggle",
    });
    expect(onCreated).toHaveBeenCalledWith({ taskId: "task-1" });
  });

  /// The correction that produced this shape: a multi-line prompt is ONE
  /// instruction to the agent. Splitting it would silently move everything
  /// after the first line into a description the agent never asked for.
  it("sends a multi-line prompt whole, and never as title plus description", async () => {
    wrap(<NewTicketModal boardId="b1" onClose={() => {}} onCreated={onCreated} />);

    await userEvent.type(box(), "Dark mode{Enter}must follow the OS setting");
    await userEvent.click(screen.getByRole("button", { name: /start drafting/i }));

    await waitFor(() => expect(posts).toHaveLength(2));
    expect(posts[1].body.seed).toBe("Dark mode\nmust follow the OS setting");
    expect(posts[0].body.description).toBeUndefined();
  });

  it("titles the placeholder from the prompt so the card is not blank", async () => {
    wrap(<NewTicketModal boardId="b1" onClose={() => {}} onCreated={onCreated} />);

    await userEvent.type(box(), "Dark mode{Enter}must follow the OS setting");
    await userEvent.click(screen.getByRole("button", { name: /start drafting/i }));

    await waitFor(() => expect(posts.length).toBeGreaterThan(0));
    // One line, whitespace collapsed — a placeholder, not a parsed title.
    expect(posts[0].body.title).toBe("Dark mode must follow the OS setting");
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

    await userEvent.type(box(), "Billing");
    await userEvent.click(screen.getByRole("button", { name: /start drafting/i }));

    await waitFor(() => expect(posts).toHaveLength(2));
    expect(posts[0].body.type).toBe("epic");
    expect(posts[1].body).toMatchObject({ kind: "decompose" });
  });

  it("carries the chosen workspace, because that is what routes the work", async () => {
    wrap(<NewTicketModal boardId="b1" onClose={() => {}} onCreated={onCreated} />);

    await userEvent.type(box(), "Fix the widget");
    // `Select` is a custom listbox (a native <select> cannot render an icon),
    // so this is click-the-trigger, click-the-option.
    await userEvent.click(await screen.findByRole("button", { name: /workspace/i }));
    await userEvent.click(await screen.findByRole("option", { name: /acme\/widgets/i }));
    await userEvent.click(screen.getByRole("button", { name: /start drafting/i }));

    await waitFor(() => expect(posts.length).toBeGreaterThan(0));
    expect(posts[0].body.workspace_id).toBe("ws-2");
  });

  /// AI-only by design: filing by hand lives in the board's own composer, and
  /// two controls that look alike would only make people choose between them.
  it("offers no manual-file escape hatch", () => {
    wrap(<NewTicketModal boardId="b1" onClose={() => {}} onCreated={onCreated} />);
    expect(screen.queryByRole("radio", { name: /file it myself/i })).toBeNull();
  });
});
