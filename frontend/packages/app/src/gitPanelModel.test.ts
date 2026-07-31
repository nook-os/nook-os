// MAIN-325: the git panel's rules. Pure functions, so each claim is tested as
// a claim rather than through a rendered panel.
import { describe, expect, it } from "vitest";
import {
  commitPaths,
  committable,
  diffFor,
  isConflict,
  splitDiffByFile,
  treeState,
} from "./gitPanelModel";

const f = (status: string, path: string) => ({ status, path });

describe("treeState", () => {
  it("is clean with no changes and uncommitted with some", () => {
    expect(treeState([])).toBe("clean");
    expect(treeState([f(" M", "a")])).toBe("uncommitted");
  });

  it("reports conflict ahead of uncommitted, because both are true", () => {
    // A conflicted tree is also dirty. Saying "3 uncommitted" is the less
    // useful of the two true things, and the one that hides the problem.
    expect(treeState([f(" M", "a"), f("UU", "b")])).toBe("conflict");
  });

  it("knows every porcelain conflict code, not just UU", () => {
    for (const code of ["DD", "AU", "UD", "UA", "DU", "AA", "UU"]) {
      expect(isConflict(f(code, "x"))).toBe(true);
    }
  });

  it("does not mistake ordinary states for conflicts", () => {
    // `A ` (added) and ` D` (deleted) each share a letter with a conflict code;
    // matching on one column would call them conflicts.
    for (const code of [" M", "M ", "A ", " D", "??", "R ", "MM", "AM"]) {
      expect(isConflict(f(code, "x"))).toBe(false);
    }
  });
});

describe("committable", () => {
  it("excludes conflicted files", () => {
    // Staging a conflicted file marks it resolved. Doing that from a checkbox,
    // with the markers still in the file, is how `<<<<<<< HEAD` reaches a
    // branch.
    const files = [f(" M", "a"), f("UU", "b"), f("??", "c")];
    expect(committable(files).map((x) => x.path)).toEqual(["a", "c"]);
  });
});

const DIFF = `diff --git a/src/one.rs b/src/one.rs
index 111..222 100644
--- a/src/one.rs
+++ b/src/one.rs
@@ -1 +1 @@
-old
+new
diff --git a/src/two.rs b/src/two.rs
index 333..444 100644
--- a/src/two.rs
+++ b/src/two.rs
@@ -1 +1 @@
-two-old
+two-new`;

describe("splitDiffByFile", () => {
  it("splits a multi-file diff by path", () => {
    const s = splitDiffByFile(DIFF);
    expect(Object.keys(s).sort()).toEqual(["src/one.rs", "src/two.rs"]);
    expect(s["src/one.rs"]).toContain("+new");
    expect(s["src/one.rs"]).not.toContain("two-new");
    expect(s["src/two.rs"]).toContain("+two-new");
  });

  it("handles a path containing spaces", () => {
    // Splitting the header on whitespace and taking a field would truncate
    // this to "my", and the file's diff would be unreachable from its row.
    const d = `diff --git a/my file.txt b/my file.txt
--- a/my file.txt
+++ b/my file.txt
@@ -1 +1 @@
-a
+b`;
    expect(Object.keys(splitDiffByFile(d))).toEqual(["my file.txt"]);
  });

  it("keys a rename by its new path", () => {
    const d = `diff --git a/old.txt b/new.txt
similarity index 90%
rename from old.txt
rename to new.txt
--- a/old.txt
+++ b/new.txt
@@ -1 +1 @@
-a
+b`;
    expect(Object.keys(splitDiffByFile(d))).toEqual(["new.txt"]);
  });

  it("is empty for an empty diff rather than throwing", () => {
    expect(splitDiffByFile("")).toEqual({});
    expect(splitDiffByFile("   \n ")).toEqual({});
  });
});

describe("diffFor", () => {
  it("returns the file's own slice", () => {
    const got = diffFor(f(" M", "src/two.rs"), splitDiffByFile(DIFF));
    expect("diff" in got && got.diff).toContain("+two-new");
  });

  it("explains an untracked file instead of showing an empty diff", () => {
    // `git diff HEAD` never mentions an untracked file, so the slice is absent
    // for a reason that is NOT "nothing changed". An empty box would say the
    // opposite of the truth.
    const got = diffFor(f("??", "brand-new.rs"), splitDiffByFile(DIFF));
    expect("reason" in got && got.reason).toMatch(/Untracked/);
  });

  it("explains a tracked file with no slice as binary or truncated", () => {
    const got = diffFor(f(" M", "image.png"), splitDiffByFile(DIFF));
    expect("reason" in got && got.reason).toMatch(/binary|truncated/);
  });
});

describe("commitPaths", () => {
  it("sends null only when the selection is the whole tree", () => {
    const files = [f(" M", "a"), f("??", "b")];
    expect(commitPaths(files, ["a", "b"])).toBeNull();
    expect(commitPaths(files, ["a"])).toEqual(["a"]);
  });

  it("NEVER sends null while a conflict exists, however complete the selection", () => {
    // The defect this exists to prevent: conflicted files are unselectable, so
    // "everything selectable is selected" is true with a smaller set. Comparing
    // against the committable files sent `null`, the node ran `git add -A`, and
    // the conflicted files went into the commit with their markers.
    const files = [f(" M", "a"), f("UU", "boom")];
    const selected = committable(files).map((x) => x.path); // a full selection
    expect(selected).toEqual(["a"]);
    expect(commitPaths(files, selected)).toEqual(["a"]);
    expect(commitPaths(files, selected)).not.toBeNull();
  });

  it("sends an empty list rather than null when nothing is selected", () => {
    // The API refuses `[]` outright; `null` would commit the whole tree.
    expect(commitPaths([f(" M", "a")], [])).toEqual([]);
  });
});
