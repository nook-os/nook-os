// MAIN-530 AC-6/AC-7: the agent surfaces render the palette through the same
// code path team chat does, and hold no command logic of their own.
//
// Driven through the REAL `ChatView` and the real `SessionChat`, because what
// the card asks about is the wiring: the list comes off the server, running one
// posts a name and the rest of the line to the surface's own endpoint, and
// slash text the server did not list is a message — which is how `/nook-spec …`
// still reaches an agent verbatim.
//
// Naming commands is allowed here for the same reason `ChatCommands.test.tsx`
// gives: a test of the palette must name what it picks, and
// `serverOwnedCommands.test.ts` is what proves nothing outside a test does.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

const SESSION = "sess-1";

const state = vi.hoisted(() => ({
  commands: [] as unknown[],
  commandResult: { data: {} } as unknown,
}));
const get = vi.hoisted(() => vi.fn());
const post = vi.hoisted(() => vi.fn());

vi.mock("@nookos/api", () => ({
  api: { GET: get, POST: post },
  READ_ONLY_POST: { "x-nook-read": "1" },
}));

import { fetchAgentCommands, runAgentCommand } from "./agentCommands";
import { SessionChat } from "./pages/SessionChat";

beforeEach(() => {
  state.commands = [
    { name: "help", args_hint: null, description: "List the commands you can use here." },
    { name: "status", args_hint: null, description: "Say what this is doing right now." },
  ];
  state.commandResult = { data: { ephemeral: "Session: running\nAgent: working" } };
  get.mockReset();
  post.mockReset();
  get.mockImplementation(async (path: string) => {
    if (path === "/api/v1/sessions/{id}/messages") return { data: [] };
    if (path.endsWith("/commands")) return { data: state.commands };
    return { data: null };
  });
  post.mockImplementation(async (path: string) =>
    path.endsWith("/commands") ? state.commandResult : { data: {} },
  );
});
afterEach(cleanup);

function renderChat() {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  return render(
    <QueryClientProvider client={qc}>
      <SessionChat sessionId={SESSION} />
    </QueryClientProvider>,
  );
}

const box = () => screen.getByLabelText("Message") as HTMLTextAreaElement;
const palette = () => screen.queryByRole("listbox", { name: "Commands" });

/** The two surfaces differ in exactly one thing — their path — which is the
 *  whole of "no per-surface command code". */
describe("the transport (AC-6)", () => {
  it("reads each surface's list from its own endpoint", async () => {
    await fetchAgentCommands("session", "s-1");
    expect(get).toHaveBeenCalledWith("/api/v1/sessions/{id}/commands", {
      params: { path: { id: "s-1" } },
    });
    await fetchAgentCommands("run", "j-1");
    expect(get).toHaveBeenCalledWith("/api/v1/jobs/{id}/commands", {
      params: { path: { id: "j-1" } },
    });
  });

  it("posts a name and the rest of the line, and nothing else", async () => {
    await runAgentCommand("run", "j-1", "status", "");
    expect(post).toHaveBeenCalledWith(
      "/api/v1/jobs/{id}/commands",
      expect.objectContaining({
        params: { path: { id: "j-1" } },
        body: { name: "status", args: "" },
      }),
    );
  });

  it("throws the server's own refusal, so the composer can render it", async () => {
    post.mockResolvedValueOnce({ error: { error: "Unknown command /nope — try /help" } });
    await expect(runAgentCommand("session", "s-1", "nope", "")).rejects.toThrow(
      "Unknown command /nope — try /help",
    );
  });
});

describe("a chat session's composer (AC-6/AC-7)", () => {
  it("offers the server's list and nothing of its own", async () => {
    renderChat();
    await userEvent.type(box(), "/");
    await waitFor(() => expect(palette()).toBeTruthy());
    expect(
      Array.from(palette()!.querySelectorAll('[role="option"]')).map((o) => o.textContent),
    ).toEqual([
      "/helpList the commands you can use here.",
      "/statusSay what this is doing right now.",
    ]);
  });

  it("runs one against the session's endpoint and shows what came back", async () => {
    renderChat();
    await waitFor(() => expect(get).toHaveBeenCalled());
    // Two presses: the first completes the highlighted row, the second sends
    // what is now in the box — the palette's own behaviour (MAIN-529 AC-4).
    await userEvent.type(box(), "/status{Enter}{Enter}");

    await waitFor(() =>
      expect(post).toHaveBeenCalledWith(
        "/api/v1/sessions/{id}/commands",
        expect.objectContaining({
          params: { path: { id: SESSION } },
          body: { name: "status", args: "" },
        }),
      ),
    );
    expect(await screen.findByText(/Agent: working/)).toBeTruthy();
    // NG-4: the answer is the reader's alone — nothing was said to the agent.
    expect(
      post.mock.calls.filter(([p]) => p === "/api/v1/sessions/{id}/messages"),
    ).toHaveLength(0);
  });

  // AC-7. The arrival of a command list must not turn other slash text into a
  // failed command: an agent is exactly the thing you address that way.
  it("sends slash text the server did not list to the agent, verbatim", async () => {
    renderChat();
    await waitFor(() => expect(get).toHaveBeenCalled());
    await userEvent.type(box(), "/nook-spec draft the migration{Enter}");

    await waitFor(() =>
      expect(post).toHaveBeenCalledWith("/api/v1/sessions/{id}/messages", {
        params: { path: { id: SESSION } },
        body: { body: "/nook-spec draft the migration" },
      }),
    );
    expect(post.mock.calls.filter(([p]) => p.endsWith("/commands"))).toHaveLength(0);
  });

  // A surface the server offers nothing on is the surface every one of these
  // was before this card: no palette, and typed text goes to the agent.
  it("offers no palette when the server lists nothing", async () => {
    state.commands = [];
    renderChat();
    await waitFor(() => expect(get).toHaveBeenCalled());
    await userEvent.type(box(), "/");
    expect(palette()).toBeNull();
  });
});
