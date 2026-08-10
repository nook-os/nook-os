// MAIN-169 AC-4/AC-5: what the login page renders while the IdP is unreachable.
// A degraded OIDC instance must show a retry notice where the IdP button sits,
// never a bare password form as its only method, and offer break-glass sign-in
// (an existing credential — never the create-owner form) only when one exists.
import React from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

// Mutable responses the mocked client serves, set per test before render.
const state = vi.hoisted(() => ({
  providers: {} as Record<string, unknown>,
  local: {} as Record<string, unknown>,
  /** Held open by a test that wants `/auth/local/status` still in flight while
   *  `/auth/providers` has answered — which is the real order (MAIN-397). */
  holdLocal: null as null | Promise<void>,
  /** Set the moment `/auth/providers` answers, so a test can wait for exactly
   *  that and not for something the first render already paints. */
  providersAnswered: false,
}));

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string) => {
      if (path === "/api/v1/auth/providers") {
        state.providersAnswered = true;
        return { data: state.providers };
      }
      if (path === "/api/v1/auth/local/status") {
        if (state.holdLocal) await state.holdLocal;
        return { data: state.local };
      }
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

afterEach(() => {
  cleanup();
  state.holdLocal = null;
  state.providersAnswered = false;
});

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

// MAIN-397: a local install has no identity provider by construction, so the
// first launch of a virgin database is account creation — and the missing
// `OIDC_*` is the expected state, never something the screen reports.
describe("Login — first run with no identity provider (MAIN-397)", () => {
  const NO_IDP = {
    oidc: false,
    oidc_degraded: false,
    dev_login: false,
    local: true,
  };
  const UNCLAIMED = {
    available: true,
    needs_bootstrap: true,
    mode: null,
    has_local_credentials: false,
  };

  it("presents account creation, and no provider button that could not work (AC-1)", async () => {
    state.providers = NO_IDP;
    state.local = UNCLAIMED;
    renderLogin();

    expect(await screen.findByText(/owns this instance/i)).toBeTruthy();
    expect(screen.getByText("Confirm password")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Create owner account" })).toBeTruthy();
    expect(screen.queryByText(/Sign in with your identity provider/i)).toBeNull();
    expect(screen.queryByRole("button", { name: "Sign in" })).toBeNull();
  });

  it("says nothing about the absent OIDC configuration (AC-3)", async () => {
    state.providers = NO_IDP;
    state.local = UNCLAIMED;
    renderLogin();

    await screen.findByText(/owns this instance/i);
    expect(screen.queryByText(/No sign-in method is configured/i)).toBeNull();
    expect(screen.queryByText(/OIDC/i)).toBeNull();
    expect(screen.queryByText(/identity provider/i)).toBeNull();
  });

  // The regression this fixes: `/auth/providers` reads config while
  // `/auth/local/status` reads (and on a virgin database writes) the default
  // tenant, so providers answers first — and the screen used to paint
  // "set OIDC_*" in that gap, about the one thing a local install is expected
  // to be missing.
  it("shows no error while the local status is still in flight (AC-3)", async () => {
    let release!: () => void;
    state.holdLocal = new Promise<void>((r) => (release = r));
    state.providers = NO_IDP;
    state.local = UNCLAIMED;
    renderLogin();

    // Wait for the providers verdict specifically — "no OIDC, no dev hatch" —
    // and let React commit it. Local is still held, so this is exactly the gap.
    await waitFor(() => expect(state.providersAnswered).toBe(true));
    await act(async () => {});
    expect(screen.queryByText(/No sign-in method is configured/i)).toBeNull();

    release();
    expect(await screen.findByText(/owns this instance/i)).toBeTruthy();
    expect(screen.queryByText(/No sign-in method is configured/i)).toBeNull();
  });

  // AC-4's other half. The server refuses a second claim outright
  // (`first_run_identity.rs`); this is what the second person actually sees.
  it("offers sign-in, not the create-owner form, once the instance is claimed (AC-4)", async () => {
    state.providers = NO_IDP;
    state.local = {
      available: true,
      needs_bootstrap: false,
      mode: "local",
      has_local_credentials: true,
    };
    renderLogin();

    expect(await screen.findByRole("button", { name: "Sign in" })).toBeTruthy();
    expect(screen.queryByText(/owns this instance/i)).toBeNull();
    expect(screen.queryByText("Confirm password")).toBeNull();
    expect(screen.queryByText(/No sign-in method is configured/i)).toBeNull();
  });
});
