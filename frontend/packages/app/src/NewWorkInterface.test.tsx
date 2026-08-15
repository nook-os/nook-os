// MAIN-502 AC-2: the New Session flow offers Chat or Terminal beside the
// runtime picker, and Chat is offered ONLY for a runtime the chosen machine
// says it can drive as one.
//
// The negative is the one that matters and the one the card names: for any
// other runtime the option is DISABLED WITH THE REASON SHOWN, never silently
// missing. A missing option reads as "this product has no chat" and sends
// people looking for a setting that does not exist.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

const NODE = "node-1";
const WORKSPACE = "ws-1";

const state = vi.hoisted(() => ({
  // What the node reports it can run, and which of those it can run as a chat.
  runtimes: ["bash", "claude"] as string[],
  chatRuntimes: ["claude"] as string[],
}));
const post = vi.hoisted(() =>
  vi.fn(async () => ({ data: { id: "sess-1", name: "claude session" } })),
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
              capabilities: {
                runtimes: state.runtimes,
                chat_runtimes: state.chatRuntimes,
              },
            },
          ],
        };
      if (path === "/api/v1/auth/me") return { data: { person_id: "p-1" } };
      if (path === "/api/v1/workspaces")
        return {
          data: {
            rows: [{ id: WORKSPACE, name: "dogfood", locations: [] }],
            next_cursor: null,
          },
        };
      if (path === "/api/v1/workspaces/{id}")
        return { data: { id: WORKSPACE, name: "dogfood", locations: [] } };
      if (path === "/api/v1/git-credentials") return { data: [] };
      if (path === "/api/v1/schedule/node") return { data: { node_id: NODE } };
      return { data: null };
    }),
    POST: post,
    PUT: vi.fn(async () => ({ data: {} })),
  },
}));

import { chatGuardText, NewWorkHost } from "./NewWorkModal";
import { useNewWork } from "./newwork";

/** Opened on an EXISTING workspace — the path that actually creates a session
 *  here, which is the only path the Interface picker claims to govern. */
function openModal() {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  useNewWork.getState().show({ workspaceId: WORKSPACE, nodeId: NODE });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <NewWorkHost />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

const chatButton = () => screen.getByRole("button", { name: "Chat" });

/** Pick a runtime by its chip, and wait for the picker to settle on it.
 *
 *  Explicit rather than relying on the flow's own preselection: that runs off
 *  the node's capabilities, which arrive a tick after the first render, and
 *  which runtime it lands on is not what this file is about. */
async function chooseRuntime(name: string) {
  // `findBy` waits: the chips come from the node's reported capabilities, which
  // land a tick after the modal first paints.
  await userEvent.click(await screen.findByRole("button", { name }));
  await waitFor(() =>
    expect(screen.getByRole("button", { name }).className).toContain("active"),
  );
}

beforeEach(() => {
  state.runtimes = ["bash", "claude"];
  state.chatRuntimes = ["claude"];
  post.mockClear();
});
afterEach(() => {
  useNewWork.getState().hide();
  cleanup();
});

describe("the Interface picker", () => {
  it("offers Chat for a runtime the node can stream", async () => {
    openModal();
    await chooseRuntime("claude");
    await waitFor(() => expect((chatButton() as HTMLButtonElement).disabled).toBe(false));
  });

  it("disables Chat for a runtime that cannot stream, and says why", async () => {
    openModal();
    await chooseRuntime("claude");
    await waitFor(() => expect((chatButton() as HTMLButtonElement).disabled).toBe(false));
    await chooseRuntime("bash");

    // Still THERE — that is the AC. Disabled, with the reason on screen.
    await waitFor(() => expect((chatButton() as HTMLButtonElement).disabled).toBe(true));
    // On screen, and naming what WOULD work (MAIN-600 AC-4) — the reason used
    // to be a `title` on a disabled button, which is the one place a browser
    // will not show it.
    const reason = await screen.findByTestId("chat-guard-reason");
    expect(reason.textContent).toContain("box offers: claude");
    expect(chatButton().getAttribute("title")).toBeNull();
  });

  it("says nothing extra once Chat is available", async () => {
    openModal();
    await chooseRuntime("claude");
    await waitFor(() => expect((chatButton() as HTMLButtonElement).disabled).toBe(false));
    expect(screen.queryByTestId("chat-guard-reason")).toBeNull();
  });

  it("sends the chosen interface when a session is created", async () => {
    openModal();
    await chooseRuntime("claude");
    await waitFor(() => expect((chatButton() as HTMLButtonElement).disabled).toBe(false));
    await userEvent.click(chatButton());
    await userEvent.click(screen.getByRole("button", { name: "start work" }));
    await waitFor(() =>
      expect(post).toHaveBeenCalledWith(
        "/api/v1/sessions",
        expect.objectContaining({
          body: expect.objectContaining({ interface: "chat" }),
        }),
      ),
    );
  });

  // Picking Chat and then a runtime that cannot be one must not leave the
  // choice standing: the server refuses that combination, so keeping it would
  // turn a runtime click into a failed create.
  it("falls back to Terminal when the runtime stops supporting chat", async () => {
    openModal();
    await chooseRuntime("claude");
    await waitFor(() => expect((chatButton() as HTMLButtonElement).disabled).toBe(false));
    await userEvent.click(chatButton());
    expect(chatButton().className).toContain("active");

    await chooseRuntime("bash");
    await waitFor(() => expect(chatButton().className).not.toContain("active"));
    expect(screen.getByRole("button", { name: "Terminal" }).className).toContain("active");
  });

  // A node that predates the field reports nothing, which must read as "no chat
  // here" rather than "chat everywhere" — the safe direction, since such a node
  // has no chat driver either.
  it("offers no chat at all on a node that does not report any", async () => {
    state.chatRuntimes = [];
    openModal();
    await chooseRuntime("claude");
    await waitFor(() => expect((chatButton() as HTMLButtonElement).disabled).toBe(true));
  });
});

// The two disabled cases the picker has, decided in one pure function so both
// are checkable without a machine, a node or a modal (MAIN-600 AC-4).
describe("chatGuardText", () => {
  it("names the machine and what it offers", () => {
    expect(
      chatGuardText({ runtime: "bash", chatRuntimes: ["claude"], nodeName: "operator" }),
    ).toBe(
      "Chat needs a runtime this machine can drive as a chat. operator offers: claude.",
    );
  });

  it("says a node reporting none offers nothing, rather than staying silent", () => {
    expect(
      chatGuardText({ runtime: "claude", chatRuntimes: [], nodeName: "operator" }),
    ).toContain("operator offers: nothing.");
  });

  // The case the tooltip never covered at all: nothing is picked, so the empty
  // list is "not asked yet" and must not be reported as a machine's answer.
  it("asks for a machine before claiming one offers nothing", () => {
    const text = chatGuardText({ runtime: "claude", chatRuntimes: [], nodeName: null });
    expect(text).toContain("no machine is picked yet");
    expect(text).not.toContain("offers:");
  });

  it("explains nothing when Chat is available", () => {
    expect(
      chatGuardText({ runtime: "claude", chatRuntimes: ["claude"], nodeName: "operator" }),
    ).toBeNull();
  });
});
