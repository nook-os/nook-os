// The signed-out invite landing (MAIN-97). Renders <AcceptInvitePage/> under a
// MemoryRouter at /accept?token=… with @nookos/api mocked — jsdom only, no
// control plane. Covers the provider matrix and, crucially, that the token
// survives every sign-in path via an encoded `next` (AC-2/AC-3), plus the
// signed-in unverified-decline message (AC-5).
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

vi.mock("@nookos/api", () => ({
  api: { GET: vi.fn(), POST: vi.fn() },
}));

import { AcceptInvitePage } from "./AcceptInvite";
import { api } from "@nookos/api";

const mock = api as unknown as {
  GET: ReturnType<typeof vi.fn>;
  POST: ReturnType<typeof vi.fn>;
};

const TOKEN = "inv_abc123";
// What `next` must encode to for the token to survive the round trip.
const NEXT = encodeURIComponent(`/accept?token=${TOKEN}`);

type Providers = { oidc: boolean; local: boolean; dev_login: boolean };

function wire({
  me = null,
  preview = null,
  providers,
  acceptResult,
}: {
  me?: unknown;
  preview?: unknown;
  providers?: Providers;
  acceptResult?: unknown;
}) {
  mock.GET.mockImplementation(async (path: string) => {
    if (path === "/api/v1/auth/me") return { data: me };
    if (path === "/api/v1/invites/preview") return { data: preview };
    if (path === "/api/v1/auth/providers") return { data: providers ?? null };
    return { data: null };
  });
  mock.POST.mockImplementation(async (path: string) => {
    if (path === "/api/v1/invites/accept")
      return { data: acceptResult, error: acceptResult ? undefined : { error: "x" } };
    return { data: null };
  });
}

function renderLanding() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={[`/accept?token=${TOKEN}`]}>
        <Routes>
          <Route path="/accept" element={<AcceptInvitePage />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  mock.GET.mockReset();
  mock.POST.mockReset();
});
afterEach(() => cleanup());

const validPreview = {
  valid: true,
  tenant: "Acme",
  inviter: "Dana",
  email: "d…@acme.test",
};

describe("AcceptInvite landing (signed out)", () => {
  it("shows who invited whom and OIDC actions that carry the token (AC-2/AC-3)", async () => {
    wire({
      preview: validPreview,
      providers: { oidc: true, local: true, dev_login: false },
    });
    renderLanding();

    // "«Inviter» invited you to «tenant»".
    expect(await screen.findByText("Dana")).toBeTruthy();
    expect(screen.getByText("Acme")).toBeTruthy();
    // The masked email is shown, never a full address.
    expect(screen.getByText("d…@acme.test")).toBeTruthy();

    // Sign in → /auth/login with the token encoded in next.
    const signIn = screen.getByText("Sign in to accept") as HTMLAnchorElement;
    expect(signIn.getAttribute("href")).toBe(`/api/v1/auth/login?next=${NEXT}`);

    // Create account → same, plus prompt=create, and NO login_hint (masked).
    const create = screen.getByText("Create an account") as HTMLAnchorElement;
    expect(create.getAttribute("href")).toBe(
      `/api/v1/auth/login?next=${NEXT}&prompt=create`,
    );
    expect(create.getAttribute("href")).not.toContain("login_hint");
  });

  it("offers a local sign-in that returns to the invite when OIDC is absent", async () => {
    wire({
      preview: validPreview,
      providers: { oidc: false, local: true, dev_login: false },
    });
    renderLanding();

    const signIn = (await screen.findByText(
      "Sign in to accept",
    )) as HTMLAnchorElement;
    // Routes to the login screen carrying next, so local login returns here.
    expect(signIn.getAttribute("href")).toBe(`/login?next=${NEXT}`);
    // No OIDC-only "Create an account" action.
    expect(screen.queryByText("Create an account")).toBeNull();
  });

  it("shows the generic invalid state for a bad token (AC-2)", async () => {
    wire({
      preview: { valid: false, tenant: "", inviter: "", email: "" },
      providers: { oidc: true, local: true, dev_login: false },
    });
    renderLanding();
    expect(
      await screen.findByText(/no longer valid/i),
    ).toBeTruthy();
    expect(screen.queryByText("Sign in to accept")).toBeNull();
  });
});

describe("AcceptInvite (signed in)", () => {
  it("surfaces the unverified-email decline explicitly (AC-5)", async () => {
    wire({
      me: { user: { email: "me@acme.test" } },
      acceptResult: {
        accepted: false,
        tenant_id: "t1",
        message: "verify your email address first, then open the invite link again",
      },
    });
    renderLanding();

    expect(
      await screen.findByText(/Verify your email to accept this invite/i),
    ).toBeTruthy();
    await waitFor(() =>
      expect(screen.getByText(/Check your inbox/i)).toBeTruthy(),
    );
  });
});
