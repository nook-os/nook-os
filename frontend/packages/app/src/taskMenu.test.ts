// MAIN-188 AC-1/AC-5: the card action menu gains the three copy-as-markdown
// items, for every task type including epics. (The exact formats are pinned in
// taskMarkdown.test.ts; this only checks the menu wiring.)
import { describe, expect, it } from "vitest";
import { taskMenuItems, type TaskMenuContext } from "./TaskMenu";
import type { TaskItem } from "@nookos/api";

const base = {
  id: "t1",
  key: "MAIN-1",
  title: "A task",
  description: "body",
  type: "task",
  priority: 2,
  url: "https://nook.example/board?task=MAIN-1",
  labels: [],
  column_id: "col1",
} as unknown as TaskItem;

const ctx = (task: TaskItem): TaskMenuContext => ({
  task,
  columns: [{ id: "col1", name: "Todo" }],
  epics: [],
  onOpen: () => {},
  onOpenLoop: () => {},
  onStartWork: () => {},
  refresh: () => {},
});

const COPY_ITEMS = ["Copy body", "Copy title + body", "Copy all (with comments)"];

describe("taskMenuItems copy-as-markdown (MAIN-188)", () => {
  it("includes all three copy scopes for a task", () => {
    const labels = taskMenuItems(ctx(base)).map((i) => i.label);
    for (const l of COPY_ITEMS) expect(labels).toContain(l);
  });

  it("includes them for an epic too (AC-5)", () => {
    const labels = taskMenuItems(ctx({ ...base, type: "epic" } as TaskItem)).map((i) => i.label);
    for (const l of COPY_ITEMS) expect(labels).toContain(l);
  });
});
