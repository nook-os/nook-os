// The ticket nook files for an undeclared repo must be BUILDABLE, not a note.
//
// The first version read as instructions to a human: no acceptance criteria, no
// non-goals, nothing to verify. `nook-build` implements `AC-N`, treats `NG-N` as
// binding and ships a PR — handed that body it had nothing to satisfy and no
// contract to open a PR against, so the ticket could only ever be research.
import { describe, expect, it } from "vitest";
import { hasPortDeclaration, portsTicketBody } from "./portSafety";

describe("the filed ports ticket is a build contract", () => {
  const body = portsTicketBody("acme/api");
  // Phrase assertions run against whitespace-normalised text: where a sentence
  // happens to wrap is formatting, and a test that breaks when a line is
  // rewrapped is pinning the wrong thing.
  const flat = body.replace(/\s+/g, " ");

  it("names the repo it is about", () => {
    expect(body).toContain("acme/api");
  });

  it("carries the sections the builder skill reads", () => {
    for (const section of [
      "## Problem",
      "## Acceptance Criteria",
      "## Non-goals",
      "## Relevant files",
      "## Test expectations",
      "## How to verify",
    ]) {
      expect(body, `missing ${section}`).toContain(section);
    }
  });

  it("has numbered, checkable criteria and binding non-goals", () => {
    expect(body).toMatch(/- \[ \] AC-1/);
    expect(body).toMatch(/- \[ \] AC-2/);
    expect(body).toMatch(/NG-1/);
  });

  it("requires the CODE change, not just the declaration", () => {
    // The half that is easy to skip. nook leases a number and sets the
    // variable; an app that ignores it collides exactly as before, and the cap
    // lifts on the declaration alone — so a ticket that only asks for
    // `.nook.toml` can be closed with the bug still in place.
    expect(flat).toMatch(/reads its port from its declared variable/i);
  });

  // ── MAIN-426: the three ways the previous body could be satisfied while
  // leaving the collision in place. Each test names the hole it closes.

  it("makes the builder SHOW the listeners they found", () => {
    // Without an inventory, partial coverage is invisible: declaring three of
    // eleven produces the same artifact as declaring all of them.
    expect(flat).toMatch(/bind-site inventory/i);
    expect(flat).toMatch(/file:line/i);
    expect(flat).toMatch(/one row per listener/i);
  });

  it("demands the second instance be EXERCISED, not merely started", () => {
    // "Both come up and neither reports a port in use" tests binding only, and
    // misses every port-DERIVED value — the vite proxy bug this repo already
    // documents would have passed it.
    expect(flat).toMatch(/exercised, not\s+merely started/i);
    expect(flat).toMatch(/port-derived values/i);
    expect(flat).toMatch(/redirect and callback urls/i);
  });

  it("rejects assertion in place of evidence, including a green suite", () => {
    expect(flat).toMatch(/"i checked" is not evidence/i);
    expect(flat).toMatch(/green test suite is not evidence/i);
  });

  it("requires a guard wired into the repo's own test command", () => {
    // Both directions, or it only half works — and it must not flag the
    // fallback literal AC-3 positively requires, or it gets suppressed
    // everywhere and protects nothing.
    expect(flat).toMatch(/guard, wired into this repo's own test command/i);
    expect(flat).toMatch(/declared `env` that nothing reads/i);
    expect(flat).toMatch(/must not flag a fallback literal/i);
  });

  it("names what the guard's consumer may be, for a non-compose repo", () => {
    expect(flat).toMatch(/compose file, the application's own bind site, a helm\s+chart, a procfile, a systemd unit/i);
  });

  it("says a fallback literal is CORRECT", () => {
    // A naive "no port literals" grep flags the `unwrap_or` arm AC-3 requires.
    // Measured on this repo: ~24 such literals, nearly all correct.
    expect(flat).toMatch(/literal is correct as the fallback/i);
  });

  it("keeps the app working outside nook", () => {
    // A repo that only starts under a lease is worse than one that hardcodes.
    expect(flat).toMatch(/variable unset the app still starts/i);
  });

  it("does not force a listener on a repo that binds none", () => {
    expect(flat).toMatch(/empty `\[\[ports\]\]` list is a valid statement/i);
  });

  it("makes an empty declaration cost evidence, not just an assertion", () => {
    // The shortcut that made the previous body worse than useless: an empty
    // list LIFTS THE CAP, so "binds nothing" was the fastest way to close the
    // ticket — and if wrong, every hardcoded port stayed with no protection.
    expect(flat).toMatch(/say so WITH the search that\s+came back empty/i);
    expect(flat).toMatch(/it is not the cheap way out/i);
    expect(flat).toMatch(/worse than leaving\s+this ticket open/i);
  });

  it("still lets an honest 'binds nothing' close cheaply", () => {
    // The evidence is a search that came back empty, not a proof of absence.
    expect(flat).toMatch(/not meant to tax the honest case/i);
    expect(flat).toMatch(/one grep and its\s+output closes this ticket/i);
  });
});

// The list badge reads the workspace ROW rather than asking
// `/reconcile-status` per repo — fine on a detail page, an N+1 on a table. It
// has to agree with the server, which derives the cap from exactly this.
describe("the list-level declaration check", () => {
  it("treats absent and null as UNDECLARED", () => {
    expect(hasPortDeclaration({})).toBe(false);
    expect(hasPortDeclaration({ port_requirements: null })).toBe(false);
  });

  it("treats an EMPTY list as a real declaration", () => {
    // "This repo binds nothing" is an answer, not a silence — the cap lifts on
    // it. Collapsing it with null would nag every repo that has honestly said
    // so, which is the whole reason the two are distinguished server-side.
    expect(hasPortDeclaration({ port_requirements: [] })).toBe(true);
  });

  it("treats a populated list as declared", () => {
    expect(
      hasPortDeclaration({ port_requirements: [{ name: "web", env: "PORT" }] }),
    ).toBe(true);
  });
});
