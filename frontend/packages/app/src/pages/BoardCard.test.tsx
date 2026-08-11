// MAIN-533 AC-8: a ticket with attachments says so on its card, so the context
// is discoverable before the ticket is opened.
import React from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import type { TaskItem } from "@nookos/api";

vi.mock("@dnd-kit/core", () => ({
  useDraggable: () => ({
    attributes: {},
    listeners: {},
    setNodeRef: () => {},
    transform: null,
    isDragging: false,
  }),
}));
vi.mock("../contextMenu", () => ({
  ContextMenuRegion: ({ children }: { children: React.ReactNode }) => <>{children}</>,
  useContextMenuApi: () => ({ openAt: () => {} }),
}));
vi.mock("@nookos/ui", () => ({
  TypeBadge: () => null,
  VisibilityBadge: () => null,
}));

import { Card } from "./Board";

const TASK = {
  id: "t1",
  key: "MAIN-1",
  title: "A ticket",
  type: "task",
  visibility: "team",
  priority: 0,
  labels: [],
  description: null,
  assignee_user_id: null,
  archived_at: null,
} as unknown as TaskItem;

afterEach(cleanup);

function show(task: TaskItem) {
  render(
    <Card task={task} onOpen={() => {}} menuItems={() => []} selected={false} blocked={false} />,
  );
}

describe("the board card's attachment count", () => {
  it("shows the count when there are attachments", () => {
    show({ ...TASK, attachment_count: 3 } as TaskItem);
    expect(screen.getByTitle("3 attachment(s)").textContent).toContain("3");
  });

  it("shows nothing at all when there are none", () => {
    show({ ...TASK, attachment_count: 0 } as TaskItem);
    expect(screen.queryByTitle(/attachment/)).toBeNull();
    // A card with no meta at all keeps the dense one-line shape it had.
    expect(document.querySelector(".card-meta")).toBeNull();
  });
});
