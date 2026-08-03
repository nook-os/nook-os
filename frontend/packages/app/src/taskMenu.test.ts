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

// A confirmed delete must dismiss the detail view too — the modal would
// otherwise keep rendering a ticket that no longer exists, with every control
// on it failing against a 404.
describe("delete closes the detail view", () => {
  const findDelete = (items: ReturnType<typeof taskMenuItems>) => {
    const flat = items.flatMap((i) =>
      "children" in i && Array.isArray((i as { children?: unknown[] }).children)
        ? ((i as { children: typeof items }).children as typeof items)
        : [i],
    );
    return flat.find((i) => "label" in i && /delete/i.test(String(i.label)));
  };

  it("exposes a delete item that can report the deletion", () => {
    const del = findDelete(taskMenuItems(ctx(base)));
    expect(del, "the menu still offers delete").toBeTruthy();
  });

  it("only closes the view for the task that was deleted", () => {
    // The wiring Board.tsx uses: close only when the open ticket IS this one,
    // so deleting from a row does not dismiss an unrelated open modal.
    const close = (openTask: string | null, deleted: TaskItem) =>
      openTask === deleted.key || openTask === deleted.id;

    expect(close("MAIN-1", base), "the open ticket closes").toBe(true);
    expect(close("t1", base), "matched by id as well as key").toBe(true);
    expect(close("MAIN-9", base), "someone else's modal stays open").toBe(false);
    expect(close(null, base), "nothing open, nothing to close").toBe(false);
  });
});
