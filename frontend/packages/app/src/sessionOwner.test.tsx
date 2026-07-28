// MAIN-137 (deferred from MAIN-133): the owner-badge rendering `SessionOwner`
// shipped without. Three cases, relative to the caller: mine → "you", someone
// else → "team", and a creator-less (legacy/MCP) row → a neutral "—". jsdom only.
import React from "react";
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { SessionOwner } from "./sessionOwner";

afterEach(cleanup);

describe("SessionOwner", () => {
  it('shows "you" for the caller\'s own session', () => {
    render(<SessionOwner createdBy="u-me" meId="u-me" />);
    expect(screen.getByText("you")).toBeTruthy();
    expect(screen.queryByText("team")).toBeNull();
  });

  it('shows "team" for someone else\'s session', () => {
    render(<SessionOwner createdBy="u-other" meId="u-me" />);
    expect(screen.getByText("team")).toBeTruthy();
    expect(screen.queryByText("you")).toBeNull();
  });

  it('shows a neutral "—" when there is no creator', () => {
    render(<SessionOwner createdBy={null} meId="u-me" />);
    expect(screen.getByText("—")).toBeTruthy();
    expect(screen.queryByText("you")).toBeNull();
    expect(screen.queryByText("team")).toBeNull();
  });
});
