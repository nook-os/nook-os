// Assembly test for the Operator page (MAIN-56).
//
// The individual pieces — DataList, SearchInput, debounce, the operator queries
// — are unit-tested elsewhere. What was only ever a manual click-through is the
// *assembled* page: that search reaches the server, that "load more" carries the
// keyset cursor, and that an action fires the right request and refetches. This
// renders the real <OperatorPage/> under react-query with `@nookos/api` and the
// dialog helpers mocked — jsdom only, no control plane (NG-2).
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";

// The page talks to exactly these two modules; stub both.
vi.mock("@nookos/api", () => ({
  api: { GET: vi.fn(), POST: vi.fn(), DELETE: vi.fn() },
}));
vi.mock("../dialogs", () => ({
  askConfirm: vi.fn(async () => true),
  askForm: vi.fn(async () => ({ email: "new@nook.test", role: "operator" })),
  askText: vi.fn(async () => "newco"),
  notify: vi.fn(async () => undefined),
}));

import { OperatorPage } from "./Operator";
import { api } from "@nookos/api";

// The stub's methods at the loose shape the tests drive them with — the real
// openapi-fetch generics are irrelevant to a mocked client.
const mock = api as unknown as {
  GET: ReturnType<typeof vi.fn>;
  POST: ReturnType<typeof vi.fn>;
  DELETE: ReturnType<typeof vi.fn>;
};

const me = { user: { email: "op@nook.test" }, capability: { operator: true } };
const tenant = (slug: string) => ({
  id: `t-${slug}`,
  slug,
  members: 1,
  nodes: 0,
  active_sessions: 0,
  workspaces: 0,
  org_id: null,
});
const listPage = (rows: unknown[], next_cursor: string | null = null) => ({
  data: { rows, next_cursor },
});

// Route each GET to a fixture, branching tenants on the search term and cursor so
// one seed exercises the plain load, a search, and paging.
function seedApi() {
  mock.GET.mockImplementation(async (path: string, opts?: { params?: { query?: Record<string, unknown> } }) => {
    const query = opts?.params?.query ?? {};
    switch (path) {
      case "/api/v1/auth/me":
        return { data: me };
      case "/api/v1/operator/orgs":
        return { data: [{ id: "o-acme", slug: "acme", name: "acme" }] };
      case "/api/v1/operator/tenants":
        if (query.q === "globex") return listPage([tenant("globex")]);
        if (query.after === "cursor1") return listPage([tenant("page-two")]);
        return listPage([tenant("alpha")], "cursor1");
      case "/api/v1/operator/nodes":
        return listPage([
          {
            id: "n-beelink",
            name: "beelink",
            tenant_slug: "acme",
            platform: "linux",
            status: "online",
            active_sessions: 0,
            last_seen_at: null,
          },
        ]);
      case "/api/v1/operator/bindings":
        return listPage([
          { id: "b-1", email: "op@nook.test", role_key: "operator", scope_type: "deployment", scope_label: null },
        ]);
      case "/api/v1/operator/audit":
        return listPage([
          { id: "a-1", occurred_at: new Date("2026-01-01").toISOString(), kind: "ca.rotate", tenant_slug: "acme", actor_type: "user" },
        ]);
      default:
        return { data: null };
    }
  });
  mock.POST.mockResolvedValue({ data: {}, error: undefined });
  mock.DELETE.mockResolvedValue({ data: {}, error: undefined });
}

// The page is sectioned now (one table on screen at a time) and the selected
// section lives in the URL, so the render needs a router and the tests need a
// way to open a section. `initial` deep-links one, exactly as a person's
// bookmark would.
function renderPage(initial = "/operator") {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <MemoryRouter initialEntries={[initial]}>
      <QueryClientProvider client={qc}>
        <OperatorPage />
      </QueryClientProvider>
    </MemoryRouter>,
  );
}

/** Open a section through the nav, like a person would. */
const openSection = async (user: ReturnType<typeof userEvent.setup>, title: string) => {
  await user.click(screen.getByRole("button", { name: title }));
};

const tenantGets = () => mock.GET.mock.calls.filter((c) => c[0] === "/api/v1/operator/tenants").length;
const nodeGets = () => mock.GET.mock.calls.filter((c) => c[0] === "/api/v1/operator/nodes").length;

beforeEach(() => {
  vi.clearAllMocks();
  seedApi();
});

// This config does not enable Vitest globals, so RTL's automatic per-test
// unmount does not register — do it explicitly, or each render's DOM leaks into
// the next test and queries match duplicates.
afterEach(() => cleanup());

describe("Operator page assembly", () => {
  it("shows tenants first and reaches every other section through the nav", async () => {
    const user = userEvent.setup();
    renderPage();
    // Tenants is the default section.
    expect(await screen.findByText("alpha")).toBeTruthy();
    // One section at a time: nodes are NOT on screen until their section is.
    expect(screen.queryByText("beelink")).toBeNull();

    await openSection(user, "Nodes");
    expect(await screen.findByText("beelink")).toBeTruthy();
    expect(screen.queryByText("alpha")).toBeNull();

    await openSection(user, "Roles");
    expect(screen.getAllByText("op@nook.test").length).toBeGreaterThan(0);

    await openSection(user, "Audit");
    expect(await screen.findByText("ca.rotate")).toBeTruthy();
  });

  it("a deep link lands on its section, and the finder narrows the nav", async () => {
    const user = userEvent.setup();
    renderPage("/operator?section=audit");
    expect(await screen.findByText("ca.rotate")).toBeTruthy();

    // The finder matches keywords, not only titles: "machine" is not in any
    // section name, but it is what somebody would type looking for Nodes.
    await user.type(screen.getByLabelText("find a section"), "machine");
    expect(await screen.findByText("beelink")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Tenants" })).toBeNull();
  });

  it("search wires q to the server and swaps in the searched rows (AC-4)", async () => {
    const user = userEvent.setup();
    renderPage();
    await screen.findByText("alpha");

    await user.type(screen.getByLabelText("Search tenants"), "globex");

    // Debounced: the request carries q, and the searched row replaces the old one.
    await waitFor(() =>
      expect(mock.GET).toHaveBeenCalledWith(
        "/api/v1/operator/tenants",
        expect.objectContaining({
          params: expect.objectContaining({ query: expect.objectContaining({ q: "globex" }) }),
        }),
      ),
    );
    expect(await screen.findByText("globex")).toBeTruthy();
    expect(screen.queryByText("alpha")).toBeNull();
  });

  it("load more wires the keyset cursor and appends the next page (AC-5)", async () => {
    const user = userEvent.setup();
    renderPage();
    await screen.findByText("alpha");
    const before = tenantGets();

    // Only the tenants list has a next cursor, so there is exactly one control.
    await user.click(await screen.findByRole("button", { name: /load more/i }));

    await waitFor(() =>
      expect(mock.GET).toHaveBeenCalledWith(
        "/api/v1/operator/tenants",
        expect.objectContaining({
          params: expect.objectContaining({ query: expect.objectContaining({ after: "cursor1" }) }),
        }),
      ),
    );
    expect(tenantGets()).toBeGreaterThan(before);
    // The second page appends: both the first and second page rows are present.
    expect(await screen.findByText("page-two")).toBeTruthy();
    expect(screen.getByText("alpha")).toBeTruthy();
  });

  it("a write action calls the API and invalidates the operator lists (AC-6)", async () => {
    const user = userEvent.setup();
    renderPage("/operator?section=nodes");
    await screen.findByText("beelink");
    const before = nodeGets();

    // askConfirm is stubbed to true, so the revoke proceeds.
    await user.click(screen.getByTitle("revoke its certificate"));

    await waitFor(() =>
      expect(mock.POST).toHaveBeenCalledWith(
        "/api/v1/operator/nodes/{id}/revoke",
        expect.objectContaining({ params: { path: { id: "n-beelink" } } }),
      ),
    );
    // bust() invalidates ["operator"], so the active nodes list refetches.
    await waitFor(() => expect(nodeGets()).toBeGreaterThan(before));
  });
});
