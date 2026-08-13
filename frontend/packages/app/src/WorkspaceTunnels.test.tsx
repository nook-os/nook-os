// The distinctions this panel exists to keep (MAIN-510).
//
// Two of them are worth naming, because both fail silently if they regress:
// "no tunnels are open" and "tunnels are not configured here" are opposite
// answers that an unchecked `?? []` renders identically, and a stop that
// skipped its confirmation would break somebody else's live URL on one
// mis-click with nothing to undo it.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

const OK = { ok: true, status: 200, statusText: "OK" } as unknown as Response;
const BAD = { ok: false, status: 400, statusText: "Bad Request" } as unknown as Response;

const OFF_MESSAGE =
  "tunnels are not enabled here — this deployment has no TUNNEL_DOMAIN, which " +
  "also needs a wildcard DNS record and a certificate for *.<domain>";

const TUNNEL = {
  label: "web-alpha-7f3",
  url: "https://web-alpha-7f3.tunnel.example",
  node_id: "n-1",
  node_name: "azul",
  port: 3000,
  session_id: "s-1",
  created_at: "2026-08-10T00:00:00Z",
  idle_secs: 120,
};

const state = vi.hoisted(() => ({
  tunnels: [] as unknown[],
  listRefused: null as string | null,
  postError: null as unknown,
}));

const get = vi.hoisted(() =>
  vi.fn(async (path: string) => {
    if (path === "/api/v1/tunnels") {
      return state.listRefused
        ? { error: { error: state.listRefused }, response: BAD }
        : { data: state.tunnels, response: OK };
    }
    if (path === "/api/v1/sessions") {
      return { data: [{ id: "s-1", name: "alpha" }], response: OK };
    }
    if (path === "/api/v1/nodes") {
      return {
        data: [
          { id: "n-1", name: "azul", status: "online" },
          { id: "n-2", name: "dark", status: "offline" },
        ],
        response: OK,
      };
    }
    return { data: [], response: OK };
  }),
);
const post = vi.hoisted(() => vi.fn());
const del = vi.hoisted(() => vi.fn());
const askConfirm = vi.hoisted(() => vi.fn(async () => true));

vi.mock("@nookos/api", () => ({
  api: { GET: get, POST: post, DELETE: del, PUT: vi.fn(), PATCH: vi.fn() },
}));
vi.mock("./dialogs", () => ({
  askConfirm,
  notify: vi.fn(async () => {}),
}));

import { WorkspaceTunnels, refusalText, sweepCountdown } from "./WorkspaceTunnels";

beforeEach(() => {
  state.tunnels = [TUNNEL];
  state.listRefused = null;
  state.postError = null;
  get.mockClear();
  post.mockReset();
  post.mockImplementation(async () =>
    state.postError ? { error: state.postError, response: BAD } : { data: TUNNEL, response: OK },
  );
  del.mockReset();
  del.mockImplementation(async () => ({ response: { ok: true, status: 204 } }));
  askConfirm.mockClear();
  askConfirm.mockResolvedValue(true);
});
afterEach(cleanup);

const renderPanel = () => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <WorkspaceTunnels workspaceId="ws-1" />
    </QueryClientProvider>,
  );
};

describe("sweepCountdown", () => {
  it("answers how long is left, not how long it has been idle (AC-4)", () => {
    expect(sweepCountdown(120, 1800).text).toBe("about 28m left");
    expect(sweepCountdown(0, 1800).text).toBe("about 30m left");
  });

  it("warns as the sweep approaches, and says so once it is due", () => {
    expect(sweepCountdown(1700, 1800)).toEqual({ text: "about 1m left", urgent: true });
    expect(sweepCountdown(1780, 1800).text).toBe("under a minute left");
    expect(sweepCountdown(1800, 1800)).toEqual({ text: "sweeping now", urgent: true });
    expect(sweepCountdown(9999, 1800).urgent).toBe(true);
    expect(sweepCountdown(120, 1800).urgent).toBe(false);
  });

  it("reads in hours for a long window, and says nothing when the sweep is off", () => {
    expect(sweepCountdown(0, 7200).text).toBe("about 2h left");
    expect(sweepCountdown(0, 7500).text).toBe("about 2h 5m left");
    // TUNNEL_IDLE_SECS=0 disables the sweep; a "0m left" there would be a lie
    // about a tunnel that is held until somebody stops it.
    expect(sweepCountdown(9999, 0).text).toBe("no idle sweep");
  });
});

describe("refusalText", () => {
  it("prefers the server's own sentence to anything assembled here", () => {
    expect(refusalText({ error: OFF_MESSAGE })).toBe(OFF_MESSAGE);
    expect(refusalText({ message: "nope" })).toBe("nope");
    expect(refusalText(undefined, BAD)).toBe("400 Bad Request");
  });
});

describe("the list", () => {
  it("renders every field a tunnel is looked up for (AC-2)", async () => {
    renderPanel();
    // Scoped to the ROW, because the node picker below it names the same
    // machines — a bare `getByText("azul")` would pass on the option alone.
    const row = within(await screen.findByTestId(`tunnel-${TUNNEL.label}`));
    expect(row.getByText(TUNNEL.url)).toBeTruthy();
    expect(row.getByText("azul")).toBeTruthy();
    expect(row.getByText("3000")).toBeTruthy();
    // The owning session by NAME — the id is what the API carries, and it is
    // not what anybody recognises their own terminal by.
    expect(row.getByText("alpha")).toBeTruthy();
    expect(row.getByText("about 28m left")).toBeTruthy();
  });

  it("shows the raw session id when the session is not this workspace's", async () => {
    state.tunnels = [{ ...TUNNEL, session_id: "s-elsewhere" }];
    renderPanel();
    expect(await screen.findByText("s-elsewhere")).toBeTruthy();
  });

  it("says no tunnels are open when the surface is on and the list is empty", async () => {
    state.tunnels = [];
    renderPanel();
    expect(await screen.findByText("No tunnels are open.")).toBeTruthy();
  });
});

describe("copying the URL (AC-3)", () => {
  it("puts the whole URL on the clipboard in one click", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    renderPanel();
    await userEvent.click(await screen.findByRole("button", { name: `copy ${TUNNEL.label} URL` }));
    expect(writeText).toHaveBeenCalledWith(TUNNEL.url);
  });
});

describe("stopping one (AC-6)", () => {
  it("confirms first, then deletes by label", async () => {
    renderPanel();
    await userEvent.click(await screen.findByRole("button", { name: `stop ${TUNNEL.label}` }));
    await waitFor(() => expect(del).toHaveBeenCalled());
    expect(askConfirm).toHaveBeenCalled();
    expect(del.mock.calls[0][1]).toEqual({ params: { path: { label: TUNNEL.label } } });
  });

  it("does nothing at all when the confirmation is declined", async () => {
    askConfirm.mockResolvedValue(false);
    renderPanel();
    await userEvent.click(await screen.findByRole("button", { name: `stop ${TUNNEL.label}` }));
    await waitFor(() => expect(askConfirm).toHaveBeenCalled());
    expect(del).not.toHaveBeenCalled();
  });

  it("shows the server's refusal verbatim (AC-8)", async () => {
    del.mockImplementation(async () => ({
      error: { error: "you may not use that machine" },
      response: BAD,
    }));
    renderPanel();
    await userEvent.click(await screen.findByRole("button", { name: `stop ${TUNNEL.label}` }));
    expect(await screen.findByTestId("tunnels-refusal").then((e) => e.textContent)).toBe(
      "you may not use that machine",
    );
  });
});

describe("opening one (AC-5)", () => {
  it("will not submit without a machine, because the API refuses a user who does not name one", async () => {
    renderPanel();
    await userEvent.type(await screen.findByLabelText("port to expose"), "3000");
    const button = screen.getByRole("button", { name: "open tunnel" });
    expect((button as HTMLButtonElement).disabled).toBe(true);
    await userEvent.click(button);
    expect(post).not.toHaveBeenCalled();
    expect(screen.getByText("say which machine to tunnel from")).toBeTruthy();
  });

  it("posts the port and the chosen node once both are given", async () => {
    renderPanel();
    await userEvent.type(await screen.findByLabelText("port to expose"), "3000");
    // Only online machines are offered: the create call refuses a node that is
    // not connected, and offering one is offering a refusal.
    await userEvent.selectOptions(screen.getByLabelText("machine to tunnel from"), "n-1");
    expect(screen.queryByRole("option", { name: "dark" })).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "open tunnel" }));
    await waitFor(() => expect(post).toHaveBeenCalled());
    expect(post.mock.calls[0][1]).toEqual({ body: { port: 3000, node_id: "n-1" } });
  });

  it("shows the server's refusal verbatim rather than guessing at it (AC-8)", async () => {
    state.postError = { error: "node dark is not connected, so a tunnel to it would answer nothing" };
    renderPanel();
    await userEvent.type(await screen.findByLabelText("port to expose"), "3000");
    await userEvent.selectOptions(screen.getByLabelText("machine to tunnel from"), "n-1");
    await userEvent.click(screen.getByRole("button", { name: "open tunnel" }));
    expect(await screen.findByTestId("tunnels-refusal").then((e) => e.textContent)).toBe(
      "node dark is not connected, so a tunnel to it would answer nothing",
    );
  });
});

describe("a deployment with no TUNNEL_DOMAIN (AC-7)", () => {
  it("says the surface is off, in the API's words, instead of showing an empty list", async () => {
    state.listRefused = OFF_MESSAGE;
    renderPanel();
    expect((await screen.findByTestId("tunnels-off")).textContent).toBe(OFF_MESSAGE);
    // The empty-list sentence is the one thing that must NOT appear: it reads
    // as "none open" when the truth is "not configured here".
    expect(screen.queryByText("No tunnels are open.")).toBeNull();
    // And no create control, which could only collect the same 400.
    expect(screen.queryByRole("button", { name: "open tunnel" })).toBeNull();
  });
});
