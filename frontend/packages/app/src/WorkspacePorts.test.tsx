// The two collisions a port declaration must not ship with (MAIN-360 AC-3).
//
// Both are enforced by the server too, and by `.nook.toml`'s parser. This layer
// exists for one reason the others cannot serve: it can point at the row you
// typed. A 400 that says "two ports called web" leaves you hunting; a message
// under the field does not.
//
// They are worth checking twice over because both fail QUIETLY if they get
// through: a duplicate `name` collides with the lease table's
// `session_port_leases_one_per_name`, and a duplicate `env` means two listeners
// write one variable and whichever loses simply has no port.
import React from "react";
import { describe, expect, it, vi, beforeEach } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { rowErrors, WorkspacePorts } from "./WorkspacePorts";

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async () => ({ data: [] })),
    PUT: vi.fn(async () => ({ data: {} })),
  },
}));

const row = (name: string, env: string) => ({
  name,
  env,
  protocol: "tcp",
  required: false,
});

describe("rowErrors", () => {
  it("passes a clean declaration", () => {
    expect(rowErrors([row("web", "PORT"), row("api", "API_PORT")])).toEqual({});
  });

  it("marks a duplicate name on the SECOND row and names the first", () => {
    // On the second, because the first is the one that is fine — flagging both
    // would leave you unsure which to change.
    const e = rowErrors([row("web", "PORT"), row("web", "OTHER")]);
    expect(e[0]).toBeUndefined();
    expect(e[1].name).toMatch(/row 1/);
  });

  it("marks a duplicate env separately from a duplicate name", () => {
    const e = rowErrors([row("web", "PORT"), row("api", "PORT")]);
    expect(e[1].env).toMatch(/row 1/);
    expect(e[1].name).toBeUndefined();
  });

  it("catches both collisions at once without confusing them", () => {
    const e = rowErrors([row("web", "PORT"), row("web", "PORT")]);
    expect(e[1].name).toBeTruthy();
    expect(e[1].env).toBeTruthy();
  });

  it("requires a name and a variable", () => {
    const e = rowErrors([row("", "")]);
    expect(e[0].name).toBeTruthy();
    expect(e[0].env).toBeTruthy();
  });

  it("refuses a variable the node could not export", () => {
    // The node splices these into the session's environment, so a space or an
    // `=` is dropped or corrupts a neighbour, with nothing pointing back here.
    for (const bad of ["MY PORT", "PORT=1", "8PORT", "PORT-2"]) {
      expect(rowErrors([row("web", bad)])[0].env, bad).toBeTruthy();
    }
    for (const ok of ["PORT", "API_PORT", "_P", "P2"]) {
      expect(rowErrors([row("web", ok)])[0], ok).toBeUndefined();
    }
  });

  it("does not invent a collision from surrounding whitespace", () => {
    // The save trims, so the check has to trim too — otherwise `web ` and `web`
    // pass here and collide at the server.
    const e = rowErrors([row("web", "PORT"), row(" web ", " PORT ")]);
    expect(e[1].name).toBeTruthy();
    expect(e[1].env).toBeTruthy();
  });

  it("treats an empty declaration as valid — it means the repo binds nothing", () => {
    expect(rowErrors([])).toEqual({});
  });
});

// The panel reads from TWO sources: `effective` is its own query, but the
// DECLARATION arrives as a prop off the workspace row — a different query, owned
// by the page. Saving used to refresh only the first, and the bug that hid
// behind that is not the stale render; it is the second edit.
//
// `useEffect` seeds the editor from `declared ?? effective`, so a stale
// `declared` WINS over the freshly-refetched effective list. Open the editor
// again and it is pre-filled with the pre-save declaration; save, and the first
// save is silently reverted. Nothing errors, and the panel looks right for the
// second or two before the workspace query would have gone stale on its own.
describe("saving refreshes both queries it reads from", () => {
  beforeEach(() => cleanup());

  const renderPanel = () => {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const invalidated: unknown[] = [];
    const real = qc.invalidateQueries.bind(qc);
    qc.invalidateQueries = ((args: { queryKey?: unknown }) => {
      invalidated.push(args?.queryKey);
      return real(args as never);
    }) as typeof qc.invalidateQueries;
    render(
      <QueryClientProvider client={qc}>
        <WorkspacePorts workspaceId="ws-1" declaredRaw={undefined} />
      </QueryClientProvider>,
    );
    return invalidated;
  };

  it("invalidates the workspace row as well as the ports query", async () => {
    const invalidated = renderPanel();
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "edit" }));
    await user.click(await screen.findByRole("button", { name: /listener/ }));
    await user.type(screen.getByLabelText("listener 1 name"), "web");
    await user.type(screen.getByLabelText("listener 1 variable"), "PORT");
    await user.click(screen.getByRole("button", { name: "save" }));

    await waitFor(() => expect(invalidated.length).toBeGreaterThan(0));
    const keys = invalidated.map((k) => JSON.stringify(k));
    // The ports query alone is what shipped; the workspace row is the fix. Both
    // are asserted so removing either one fails here rather than in a browser.
    expect(keys).toContain(JSON.stringify(["workspace-ports", "ws-1"]));
    expect(keys).toContain(JSON.stringify(["workspaces", "ws-1"]));
  });
});

// An owner marking a frontend without touching the repo (MAIN-596 AC-6). The
// assertion is on what reaches the API rather than on the checkbox, because the
// checkbox rendering and the field being SENT are two different failures and
// only the second one loses the setting.
describe("marking a listener browsable", () => {
  beforeEach(() => cleanup());

  it("sends browsable and its path in the declaration", async () => {
    const { api } = (await import("@nookos/api")) as unknown as {
      api: { PUT: ReturnType<typeof vi.fn> };
    };
    api.PUT.mockClear();
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <WorkspacePorts workspaceId="ws-1" declaredRaw={undefined} />
      </QueryClientProvider>,
    );
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "edit" }));
    await user.click(await screen.findByRole("button", { name: /listener/ }));
    await user.type(screen.getByLabelText("listener 1 name"), "admin");
    await user.type(screen.getByLabelText("listener 1 variable"), "ADMIN_PORT");
    // The path is inert until the listener is browsable, so it cannot be typed
    // into first — which is the point of disabling it rather than hiding it.
    const path = screen.getByLabelText("listener 1 path") as HTMLInputElement;
    expect(path.disabled).toBe(true);
    await user.click(screen.getByLabelText("listener 1 browsable"));
    await user.clear(path);
    await user.type(path, "/admin");
    await user.click(screen.getByRole("button", { name: "save" }));

    await waitFor(() => expect(api.PUT).toHaveBeenCalled());
    expect(api.PUT.mock.calls[0][1].body.requirements).toEqual([
      { name: "admin", env: "ADMIN_PORT", protocol: "tcp", required: false, browsable: true, path: "/admin" },
    ]);
  });
});
