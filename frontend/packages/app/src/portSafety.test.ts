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
    expect(flat).toMatch(/reads its port from that variable/i);
    expect(flat).toMatch(/No hardcoded port literal/i);
  });

  it("demands proof that two instances coexist", () => {
    expect(flat).toMatch(/Two instances run on one machine at once/i);
  });

  it("keeps the app working outside nook", () => {
    // A repo that only starts under a lease is worse than one that hardcodes.
    expect(flat).toMatch(/variable unset the app still starts/i);
  });

  it("does not force a listener on a repo that binds none", () => {
    expect(flat).toMatch(/empty `\[\[ports\]\]` list is a valid statement/i);
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
