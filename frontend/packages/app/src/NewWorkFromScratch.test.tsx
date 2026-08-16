// MAIN-619: "+ New Workspace" used to be a repo-URL deploy form. It already
// DERIVED the from-scratch intent from a plain name — and then refused to
// submit it, because `canGo` demanded a URL. These are the four things that
// change when what you typed is a name rather than a URL, and the one thing
// that must not: a git URL still deploys exactly as it did.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

const NODE = "node-1";
const WORKSPACE = "ws-1";

// One loose shape for every endpoint the flow touches, so a case that mocks a
// refusal or an error is not fighting the union TS infers from the happy path.
type Reply = { data?: unknown; error?: unknown };

const post = vi.hoisted(() =>
  vi.fn(async (path: string): Promise<Reply> => {
    if (path === "/api/v1/nodes/{id}/projects")
      return { data: { ok: true, path: "/w/greeting-lab", message: "created project" } };
    if (path === "/api/v1/sessions") return { data: { id: "sess-1", name: "claude session" } };
    return { data: {} };
  }),
);

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string) => {
      if (path === "/api/v1/nodes")
        return {
          data: [
            {
              id: NODE,
              name: "box",
              platform: "linux",
              status: "online",
              shared: false,
              owner_person_id: "p-1",
              capabilities: { runtimes: ["bash", "claude"], chat_runtimes: [] },
            },
          ],
        };
      if (path === "/api/v1/auth/me") return { data: { person_id: "p-1" } };
      if (path === "/api/v1/workspaces")
        return {
          data: {
            rows: [{ id: WORKSPACE, name: "greeting-lab", locations: [] }],
            next_cursor: null,
          },
        };
      if (path === "/api/v1/workspaces/{id}")
        return { data: { id: WORKSPACE, name: "greeting-lab", locations: [] } };
      if (path === "/api/v1/git-credentials") return { data: [] };
      if (path === "/api/v1/schedule/node") return { data: { node_id: NODE } };
      return { data: null };
    }),
    POST: post,
    PUT: vi.fn(async () => ({ data: {} })),
  },
}));

// The vault asks for the app password before it seals anything; nothing here
// pastes a .env, and the adopt path must not open a prompt in a test.
vi.mock("./envvault", () => ({
  saveEnv: vi.fn(async () => true),
  adoptEnvFromDisk: vi.fn(async () => {}),
}));

import { NewWorkHost } from "./NewWorkModal";
import { useNewWork } from "./newwork";

/** The top-bar open: no workspace, no task — the form this card is about. */
function openModal() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: 0 } } });
  useNewWork.getState().show();
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <NewWorkHost />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

const input = () => screen.getByPlaceholderText(/greeting-lab/);
const createButton = () => screen.getByRole("button", { name: /^(Create|Create & deploy)$/ });

beforeEach(() => post.mockClear());
afterEach(() => {
  useNewWork.getState().hide();
  cleanup();
});

describe("from-scratch mode", () => {
  // AC-1. The gate that made the whole intent unreachable.
  it("enables Create for a plain name and keeps it disabled on empty input", async () => {
    openModal();
    expect((createButton() as HTMLButtonElement).disabled).toBe(true);
    await userEvent.type(input(), "greeting-lab");
    await waitFor(() => expect((createButton() as HTMLButtonElement).disabled).toBe(false));
    expect(createButton().textContent).toBe("Create");
  });

  // AC-1. "enter a git URL" described a name as a malformed URL; the chip now
  // names the other thing this form makes.
  it("derives `new project` for a name and `deploy` for a git URL", async () => {
    openModal();
    await userEvent.type(input(), "greeting-lab");
    expect((await screen.findByTestId("new-work-intent")).textContent).toBe("new project");

    await userEvent.clear(input());
    await userEvent.type(input(), "git@github.com:org/repo.git");
    await waitFor(() => expect(screen.queryByTestId("new-work-intent")).toBeNull());
    expect(screen.getByText("→ deploy")).toBeTruthy();
  });

  // AC-2. Hidden on the deploy path because the reconciler places it; a project
  // is written on ONE machine, so which machine is the operator's to override.
  it("shows an overridable node select, and hides it again for a URL", async () => {
    openModal();
    await userEvent.type(input(), "greeting-lab");
    const select = (await screen.findByTestId("new-work-node")) as HTMLSelectElement;
    expect(Array.from(select.options).some((o) => o.textContent?.includes("box"))).toBe(true);
    await userEvent.selectOptions(select, NODE);
    expect(select.value).toBe(NODE);

    await userEvent.clear(input());
    await userEvent.type(input(), "https://github.com/org/repo");
    await waitFor(() => expect(screen.queryByTestId("new-work-node")).toBeNull());
  });

  // AC-10. The cap exists because a repo has not said what it binds, and this
  // one is created having said it — so the warning would be false.
  it("swaps the port-cap warning for the note about the file being created", async () => {
    openModal();
    await userEvent.type(input(), "greeting-lab");
    const note = await screen.findByTestId("new-work-port-note");
    expect(note.textContent).toContain(".nook.toml");
    expect(note.textContent).toContain("is created for you");
    expect(note.textContent).not.toContain("limited to one session per");

    await userEvent.clear(input());
    await userEvent.type(input(), "https://github.com/org/repo");
    await waitFor(() =>
      expect(screen.getByTestId("new-work-port-note").textContent).toContain(
        "limited to one session per",
      ),
    );
  });

  // AC-2, AC-7, AC-9: the create call carries the typed description, and the
  // session that follows opens in the primary checkout — no worktree (NG-4).
  it("creates the project with its description and opens a session in the checkout", async () => {
    openModal();
    await userEvent.type(input(), "greeting-lab");
    await userEvent.type(
      await screen.findByPlaceholderText(/A scratch repo for trying/),
      "A place to try greetings.",
    );
    await waitFor(() => expect((createButton() as HTMLButtonElement).disabled).toBe(false));
    await userEvent.click(createButton());

    await waitFor(() =>
      expect(post).toHaveBeenCalledWith(
        "/api/v1/nodes/{id}/projects",
        expect.objectContaining({
          body: { name: "greeting-lab", description: "A place to try greetings." },
        }),
      ),
    );
    await waitFor(() =>
      expect(post).toHaveBeenCalledWith(
        "/api/v1/sessions",
        expect.objectContaining({
          body: expect.objectContaining({ workspace_id: WORKSPACE, path: null }),
        }),
      ),
    );
    // NG-4: nothing asked for a worktree on the way.
    expect(post.mock.calls.map((c) => c[0])).not.toContain(
      "/api/v1/workspaces/{id}/worktrees",
    );
  });

  // AC-7: omitted is not blank. The node distinguishes them, so the modal must
  // not send an empty string that would read as "described as nothing".
  it("sends no description when the field is left empty", async () => {
    openModal();
    await userEvent.type(input(), "greeting-lab");
    await waitFor(() => expect((createButton() as HTMLButtonElement).disabled).toBe(false));
    await userEvent.click(createButton());
    await waitFor(() =>
      expect(post).toHaveBeenCalledWith(
        "/api/v1/nodes/{id}/projects",
        expect.objectContaining({ body: { name: "greeting-lab", description: null } }),
      ),
    );
  });

  // AC-11. The node knows why it refused; the modal reports that sentence and
  // stays open with what you typed.
  it("surfaces the node's own refusal and keeps the input", async () => {
    post.mockImplementation(async (path: string): Promise<Reply> => {
      if (path === "/api/v1/nodes/{id}/projects")
        return { data: { ok: false, path: null, message: "/w/greeting-lab already exists" } };
      return { data: {} };
    });
    openModal();
    await userEvent.type(input(), "greeting-lab");
    await waitFor(() => expect((createButton() as HTMLButtonElement).disabled).toBe(false));
    await userEvent.click(createButton());

    expect(await screen.findByText("/w/greeting-lab already exists")).toBeTruthy();
    expect((input() as HTMLInputElement).value).toBe("greeting-lab");
    expect(post.mock.calls.map((c) => c[0])).not.toContain("/api/v1/sessions");
  });

  // AC-12. The repo is real by then; a failure after it must not strand it
  // behind a modal whose only other button is "cancel".
  it("offers the new workspace when the session start fails after the project exists", async () => {
    post.mockImplementation(async (path: string): Promise<Reply> => {
      if (path === "/api/v1/nodes/{id}/projects")
        return { data: { ok: true, path: "/w/greeting-lab", message: "created project" } };
      if (path === "/api/v1/sessions") return { error: { message: "node is offline" } };
      return { data: {} };
    });
    openModal();
    await userEvent.type(input(), "greeting-lab");
    await waitFor(() => expect((createButton() as HTMLButtonElement).disabled).toBe(false));
    await userEvent.click(createButton());

    expect(await screen.findByRole("button", { name: "open the workspace" })).toBeTruthy();
  });
});

// NG-3: the clone path is untouched. Typing a URL still writes a workspace and
// a session SPEC and hands placement to the reconciler — it never calls the
// project endpoint.
describe("the deploy path", () => {
  it("still declares a workspace and its session spec", async () => {
    post.mockImplementation(async (path: string): Promise<Reply> => {
      if (path === "/api/v1/workspaces") return { data: { id: WORKSPACE } };
      return { data: {} };
    });
    openModal();
    await userEvent.type(input(), "git@github.com:org/repo.git");
    await waitFor(() => expect(createButton().textContent).toBe("Create & deploy"));
    await userEvent.click(createButton());

    await waitFor(() => expect(post).toHaveBeenCalledWith("/api/v1/workspaces", expect.anything()));
    expect(post.mock.calls.map((c) => c[0])).not.toContain("/api/v1/nodes/{id}/projects");
  });
});
