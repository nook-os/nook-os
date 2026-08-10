// The workspace context strip (MAIN-488).
//
// The count is the assertion: four tabs, of which only one was workspace-scoped,
// became three that all are. A regression here is somebody adding a global
// destination back to a strip that exists to say "the repo you picked".
import React from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";

vi.mock("@nookos/api", () => ({
  api: { GET: vi.fn(async () => ({ data: null })) },
  listChannels: vi.fn(async () => []),
  listDms: vi.fn(async () => []),
}));

import { useWorkspaceContext } from "./context";
import { ContextTabs } from "./layout";

beforeEach(() => {
  cleanup();
  useWorkspaceContext.setState({ selectedWorkspaceId: "ws-1" });
});

function renderTabs(url: string) {
  return render(
    <MemoryRouter initialEntries={[url]}>
      <ContextTabs />
    </MemoryRouter>,
  );
}

const tabs = () => Array.from(document.querySelectorAll("a.nook-tab")) as HTMLAnchorElement[];
const active = () => tabs().filter((t) => t.classList.contains("active")).map((t) => t.textContent);

describe("ContextTabs", () => {
  it("shows exactly Overview, Sessions and Runs", () => {
    renderTabs("/workspaces/ws-1");
    expect(tabs().map((t) => t.textContent)).toEqual(["Overview", "Sessions", "Runs"]);
  });

  it("points Runs at the workspace's runs section", () => {
    renderTabs("/workspaces/ws-1");
    const runs = tabs().find((t) => t.textContent === "Runs");
    expect(runs?.getAttribute("href")).toBe("/workspaces/ws-1?section=runs");
  });

  it("marks Runs active on the runs section, and Overview on every other", () => {
    renderTabs("/workspaces/ws-1?section=runs");
    expect(active()).toEqual(["Runs"]);
    cleanup();
    renderTabs("/workspaces/ws-1?section=checkouts");
    expect(active()).toEqual(["Overview"]);
    cleanup();
    renderTabs("/workspaces/ws-1");
    expect(active()).toEqual(["Overview"]);
  });

  it("asks for a workspace before offering any of them", () => {
    useWorkspaceContext.setState({ selectedWorkspaceId: null });
    renderTabs("/workspaces");
    expect(tabs()).toHaveLength(0);
    expect(screen.getByText(/pick a workspace to focus/i)).toBeTruthy();
  });
});
