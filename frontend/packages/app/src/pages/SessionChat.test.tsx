// MAIN-502 web surface: a chat session's conversation, wired to a mocked
// control-plane client. jsdom only, no control plane.
//
// What is pinned here is what the card's "How to verify" actually asks a person
// to do: the history renders as a conversation, typing sends, a permission
// request appears IN the log with its choices beside the composer, and
// answering posts the verdict. Plus the two negatives — a request that has
// already been answered offers no buttons, and the composer is held shut while
// the agent is blocked.
//
// MAIN-620 adds the third choice: "allow always", which posts `remember` so the
// node stops asking about that tool for the rest of the session.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

const SESSION = "sess-1";

const state = vi.hoisted(() => ({ messages: [] as unknown[] }));
const post = vi.hoisted(() => vi.fn(async () => ({ data: {} })));

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string) => {
      if (path === "/api/v1/sessions/{id}/messages") return { data: state.messages };
      return { data: null };
    }),
    POST: post,
  },
}));

import { SessionChat, chatMessages, outstandingPermission } from "./SessionChat";

function msg(over: Record<string, unknown> = {}) {
  return {
    id: `m-${Math.random()}`,
    session_id: SESSION,
    role: "agent",
    body: "",
    permission_request_id: null,
    tool_name: null,
    decision: null,
    at: "2026-08-10T10:00:00Z",
    ...over,
  };
}

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

beforeEach(() => {
  state.messages = [];
  post.mockClear();
});
afterEach(cleanup);

describe("the conversation", () => {
  it("renders both sides of the exchange", async () => {
    state.messages = [
      msg({ role: "human", body: "add a greeting command" }),
      msg({ role: "agent", body: "Reading the code." }),
    ];
    renderChat();
    expect(await screen.findByText("add a greeting command")).toBeTruthy();
    expect(await screen.findByText("Reading the code.")).toBeTruthy();
  });

  // The same fold the loop's transcript uses (MAIN-499): a ladder of identical
  // tool lines is one activity entry, not seven rows of `· Bash`.
  it("folds a run of tool markers into one activity line", () => {
    const folded = chatMessages([
      msg({ role: "agent", body: "· Bash" }),
      msg({ role: "agent", body: "· Bash" }),
      msg({ role: "agent", body: "· Read" }),
      msg({ role: "agent", body: "Done." }),
    ] as never);
    expect(folded).toHaveLength(2);
    expect(folded[0].body).toBe("· 3 steps — Bash ×2 · Read");
    // The steps survive the fold, so the reader can open it (MAIN-499 AC-5).
    expect(folded[0].activity).toEqual(["· Bash", "· Bash", "· Read"]);
    expect(folded[1].body).toBe("Done.");
  });

  it("sends what was typed", async () => {
    renderChat();
    const box = await screen.findByRole("textbox");
    await userEvent.type(box, "make it say hi");
    await userEvent.click(screen.getByRole("button", { name: /send/i }));
    await waitFor(() =>
      expect(post).toHaveBeenCalledWith(
        "/api/v1/sessions/{id}/messages",
        expect.objectContaining({ body: { body: "make it say hi" } }),
      ),
    );
  });
});

describe("a permission request", () => {
  const blocked = () =>
    msg({
      role: "permission",
      body: "rm -rf build/",
      tool_name: "Bash",
      permission_request_id: "req-1",
    });

  // AC-6: it is a MESSAGE with its choices — in the log, not a strip bolted
  // to the side of it. MAIN-620 AC-3 makes those choices three.
  it("appears in the log, with allow once, allow all <tool> and deny", async () => {
    state.messages = [blocked()];
    renderChat();
    expect(
      await screen.findByText(/Permission needed — Bash: rm -rf build\//),
    ).toBeTruthy();
    const choices = await screen.findByTestId("permission-choices");
    expect(choices.textContent).toContain("Allow once");
    // The tool is NAMED, because the tap grants the tool and not the command
    // the request happens to be about.
    expect(choices.textContent).toContain("Allow all Bash");
    expect(choices.textContent).toContain("Deny");
  });

  it("posts the verdict it was given", async () => {
    state.messages = [blocked()];
    renderChat();
    await userEvent.click(await screen.findByRole("button", { name: "Deny" }));
    await waitFor(() =>
      expect(post).toHaveBeenCalledWith(
        "/api/v1/sessions/{id}/permissions/{request_id}",
        expect.objectContaining({
          params: { path: { id: SESSION, request_id: "req-1" } },
          body: { allow: false, remember: false },
        }),
      ),
    );
  });

  // MAIN-620 AC-3. The suppression itself is the node's — it holds the set and
  // answers the tool without announcing it — so what this surface owes is the
  // distinction: "once" and "always" must not post the same thing, or the node
  // has nothing to tell them apart by and the button is decorative.
  it("asks for the tool to be remembered when told always", async () => {
    state.messages = [blocked()];
    renderChat();
    await userEvent.click(
      await screen.findByRole("button", { name: "Allow all Bash" }),
    );
    await waitFor(() =>
      expect(post).toHaveBeenCalledWith(
        "/api/v1/sessions/{id}/permissions/{request_id}",
        expect.objectContaining({
          params: { path: { id: SESSION, request_id: "req-1" } },
          body: { allow: true, remember: true },
        }),
      ),
    );
  });

  it("asks for nothing to be remembered when told once", async () => {
    state.messages = [blocked()];
    renderChat();
    await userEvent.click(
      await screen.findByRole("button", { name: "Allow once" }),
    );
    await waitFor(() =>
      expect(post).toHaveBeenCalledWith(
        "/api/v1/sessions/{id}/permissions/{request_id}",
        expect.objectContaining({
          body: { allow: true, remember: false },
        }),
      ),
    );
  });

  // The agent is BLOCKED, so there is nothing to say to it until it is
  // answered. An open composer there invites a message that would sit unread.
  it("holds the composer shut while the agent waits", async () => {
    state.messages = [blocked()];
    renderChat();
    // The composer renders before the conversation arrives, so wait for the
    // request to be on screen — that is the moment the box must be shut.
    await screen.findByTestId("permission-choices");
    const box = screen.getByRole("textbox") as HTMLTextAreaElement;
    expect(box.disabled).toBe(true);
  });

  // …and once answered — by this device or the other one — the buttons go and
  // the log says what was decided.
  it("offers no buttons for a request that is already settled", async () => {
    state.messages = [{ ...blocked(), decision: "allow" }];
    renderChat();
    expect(await screen.findByText(/Allowed\./)).toBeTruthy();
    expect(screen.queryByTestId("permission-choices")).toBeNull();
  });

  // The runtime asks about one tool at a time, so an older unanswered row is
  // one whose agent has gone — a node that died holding it. Offering buttons
  // for that would address a process that is not there.
  it("offers the newest outstanding request, never an abandoned one", () => {
    const stale = { ...blocked(), permission_request_id: "req-old" };
    const live = { ...blocked(), permission_request_id: "req-new" };
    expect(outstandingPermission([stale, live] as never)?.permission_request_id).toBe(
      "req-new",
    );
    expect(outstandingPermission([] as never)).toBeNull();
    expect(
      outstandingPermission([{ ...blocked(), decision: "deny" }] as never),
    ).toBeNull();
  });
});
