// MAIN-169 AC-4/AC-5: what the login page renders while the IdP is unreachable.
// A degraded OIDC instance must show a retry notice where the IdP button sits,
// never a bare password form as its only method, and offer break-glass sign-in
// (an existing credential — never the create-owner form) only when one exists.
import React from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

// Mutable responses the mocked client serves, set per test before render.
const state = vi.hoisted(() => ({
  providers: {} as Record<string, unknown>,
  local: {} as Record<string, unknown>,
}));

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string) => {
      if (path === "/api/v1/auth/providers") return { data: state.providers };
      if (path === "/api/v1/auth/local/status") return { data: state.local };
      if (path === "/api/v1/auth/dev-accounts") return { data: [] };
      return { data: null };
    }),
    POST: vi.fn(async () => ({ error: null, response: { ok: true } })),
  },
}));

import { Login } from "./Login";

function renderLogin() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <Login />
    </QueryClientProvider>,
  );
}

afterEach(() => cleanup());

describe("Login — OIDC degraded (MAIN-169)", () => {
  it("shows the retry notice, no IdP button, and no bare form when no local credential exists", async () => {
    state.providers = { oidc: false, oidc_degraded: true, dev_login: false, local: true };
    state.local = {
      available: false,
      needs_bootstrap: false,
      mode: "oidc",
      has_local_credentials: false,
    };
    renderLogin();

    expect(await screen.findByText(/Identity provider unreachable/i)).toBeTruthy();
    // The IdP button is gone (replaced by the notice)…
    expect(screen.queryByText(/Sign in with your identity provider/i)).toBeNull();
    // …and there is NO password form standing in as the only method.
    expect(screen.queryByText("Username")).toBeNull();
    // The degraded outage is not "nothing configured".
    expect(screen.queryByText(/No sign-in method is configured/i)).toBeNull();
    // The break-glass absence is explained.
    expect(
      await screen.findByText(/No local account exists on this instance/i),
    ).toBeTruthy();
  });

  it("offers break-glass sign-in — not the create-owner form — when a local credential exists", async () => {
    state.providers = { oidc: false, oidc_degraded: true, dev_login: false, local: true };
    state.local = {
      available: false,
      needs_bootstrap: false,
      mode: "oidc",
      has_local_credentials: true,
    };
    renderLogin();

    expect(await screen.findByText(/Identity provider unreachable/i)).toBeTruthy();
    // The password form appears…
    expect(await screen.findByText("Username")).toBeTruthy();
    // …as a sign-in, never a bootstrap: no owner-claim copy, no confirm field.
    expect(screen.queryByText(/owns this instance/i)).toBeNull();
    expect(screen.queryByText("Confirm password")).toBeNull();
    expect(screen.getByRole("button", { name: "Sign in" })).toBeTruthy();
    expect(screen.queryByText("Create owner account")).toBeNull();
  });

  it("shows the IdP button and no notice when OIDC is healthy", async () => {
    state.providers = { oidc: true, oidc_degraded: false, dev_login: false, local: false };
    state.local = {
      available: false,
      needs_bootstrap: false,
      mode: "oidc",
      has_local_credentials: false,
    };
    renderLogin();

    expect(
      await screen.findByText(/Sign in with your identity provider/i),
    ).toBeTruthy();
    expect(screen.queryByText(/Identity provider unreachable/i)).toBeNull();
  });
});
