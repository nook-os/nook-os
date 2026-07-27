// Assembly test for the Team page (MAIN-64). The reused pieces (DataList,
// SearchInput, the member queries) are tested elsewhere; what is new here is the
// permission gating (AC-3) and the invite action feedback (AC-6/AC-7). Renders
// the real <TeamPage/> under react-query with @nookos/api and the app's toast /
// dialog / context helpers mocked — jsdom only, no control plane.
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

vi.mock("@nookos/api", () => ({
  api: { GET: vi.fn(), POST: vi.fn(), DELETE: vi.fn(), PATCH: vi.fn() },
}));
const pushSpy = vi.fn();
vi.mock("../Notifications", () => ({
  useToasts: { getState: () => ({ push: pushSpy }) },
}));
vi.mock("../context", () => ({ useWorkspaceContext: () => ({ select: vi.fn() }) }));
vi.mock("../dialogs", () => ({
  askConfirm: vi.fn(async () => true),
  notify: vi.fn(),
  CopyRow: ({ value }: { value: string }) => <div data-testid="copy-row">{value}</div>,
}));

import { TeamPage, expiryLabel } from "./Team";
import { api } from "@nookos/api";

const mock = api as unknown as {
  GET: ReturnType<typeof vi.fn>;
  POST: ReturnType<typeof vi.fn>;
  DELETE: ReturnType<typeof vi.fn>;
  PATCH: ReturnType<typeof vi.fn>;
};

const meWith = (role: string) => ({
  user: { id: "me" },
  tenant: { id: "t1", name: "Acme" },
  tenants: [
    { id: "t1", name: "Acme", slug: "acme", role, current: true },
    { id: "t2", name: "Beta", slug: "beta", role: "member", current: false },
  ],
});

const members = {
  rows: [
    { principal_id: "p1", display_name: "Alice", email: "alice@x.test", role: "admin" },
    { principal_id: "me", display_name: "Me", email: "me@x.test", role: "owner" },
  ],
  next_cursor: null,
};

function wireGet(role: string, invites: unknown[] = []) {
  mock.GET.mockImplementation(async (path: string) => {
    if (path === "/api/v1/auth/me") return { data: meWith(role) };
    if (path === "/api/v1/tenants/{id}/members") return { data: members };
    if (path === "/api/v1/tenants/{id}/invites") return { data: invites };
    return { data: null };
  });
}

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <TeamPage />
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  pushSpy.mockClear();
  mock.GET.mockReset();
  mock.POST.mockReset();
  mock.DELETE.mockReset();
});
afterEach(() => cleanup());

describe("TeamPage", () => {
  it("shows the roster read-only and hides Invites for a plain member (AC-3)", async () => {
    wireGet("member");
    renderPage();
    // Roster renders.
    expect(await screen.findByText("Alice")).toBeTruthy();
    // "Your organizations" is always shown.
    expect(screen.getByText("Your organizations")).toBeTruthy();
    // No Invites panel for a member.
    expect(screen.queryByText("Invites")).toBeNull();
    // Read-only roster: no role <select> and no Remove button.
    expect(screen.queryByRole("combobox")).toBeNull();
    expect(screen.queryByText("Remove")).toBeNull();
  });

  it("shows Invites for an owner and toasts on create (AC-6/AC-7)", async () => {
    wireGet("owner");
    mock.POST.mockResolvedValue({
      data: {
        id: "inv1",
        email: "new@x.test",
        role: "member",
        accept_url: "https://nook.test/accept?token=xyz",
        expires_at: "2026-08-10T00:00:00Z",
      },
      response: { ok: true, status: 200 },
    });
    renderPage();

    // Owner sees the Invites panel.
    expect(await screen.findByText("Invites")).toBeTruthy();

    // The composer is always visible now (AC-4) — no "+ invite" toggle to open.
    await userEvent.type(screen.getByPlaceholderText("person@example.com"), "new@x.test");
    await userEvent.click(screen.getByRole("button", { name: "invite" }));

    // Success toast pushed through the shared store (no modal).
    await waitFor(() =>
      expect(pushSpy).toHaveBeenCalledWith(
        expect.objectContaining({ level: "success", title: "Invite sent to new@x.test" }),
      ),
    );
  });

  it("does not claim success when a revoke fails (AC-7)", async () => {
    // A pending invite is present so its row (and revoke button) renders.
    wireGet("owner", [
      { id: "inv9", email: "pending@x.test", role: "member", expires_at: "2026-08-10T00:00:00Z" },
    ]);
    // openapi-fetch surfaces a failure as `error`, not a throw.
    mock.DELETE.mockResolvedValue({
      error: { error: "boom" },
      response: { ok: false, status: 500 },
    });
    renderPage();

    // askConfirm is mocked to true, so clicking revoke proceeds to the DELETE.
    const revoke = await screen.findByTitle("revoke");
    await userEvent.click(revoke);

    await waitFor(() => expect(mock.DELETE).toHaveBeenCalled());
    // The failure must NOT masquerade as success — no "Invite revoked" toast.
    expect(pushSpy).not.toHaveBeenCalledWith(
      expect.objectContaining({ title: "Invite revoked" }),
    );
  });
});

// The pure relative-expiry formatter (MAIN-121 AC-5). A reference `now` is
// passed in, so these are deterministic — no wall clock.
describe("expiryLabel", () => {
  const NOW = Date.parse("2026-07-27T12:00:00Z");
  const H = 3_600_000;
  const D = 24 * H;
  const at = (deltaMs: number) => new Date(NOW + deltaMs).toISOString();

  it("shows days with a muted tone when a day or more remains", () => {
    expect(expiryLabel(at(6 * D), NOW)).toEqual({ text: "expires in 6d", tone: "muted" });
    expect(expiryLabel(at(3 * D + 5 * H), NOW)).toEqual({
      text: "expires in 3d",
      tone: "muted",
    });
  });

  it("shows hours with a warn tone under 24h", () => {
    expect(expiryLabel(at(3 * H), NOW)).toEqual({ text: "expires in 3h", tone: "warn" });
    expect(expiryLabel(at(23 * H), NOW)).toEqual({ text: "expires in 23h", tone: "warn" });
  });

  it("treats exactly 24h as a day — the warn/day boundary", () => {
    expect(expiryLabel(at(24 * H), NOW)).toEqual({ text: "expires in 1d", tone: "muted" });
    // Just under 24h stays in the warn band and never rounds up to "24h".
    expect(expiryLabel(at(24 * H - 1), NOW)).toEqual({
      text: "expires in 23h",
      tone: "warn",
    });
  });

  it("clamps a sub-hour invite to 1h rather than 0h", () => {
    expect(expiryLabel(at(30 * 60_000), NOW)).toEqual({
      text: "expires in 1h",
      tone: "warn",
    });
  });

  it("reports expired (err) at and past expiry", () => {
    expect(expiryLabel(at(0), NOW)).toEqual({ text: "expired", tone: "err" });
    expect(expiryLabel(at(-5 * H), NOW)).toEqual({ text: "expired", tone: "err" });
  });
});
