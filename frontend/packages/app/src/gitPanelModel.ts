// The git panel's rules, as pure functions (MAIN-325).
//
// Split out from `Session.tsx` because each of them is a claim that can be
// wrong in a way a screenshot would not reveal: which state the tree is in,
// which slice of a unified diff belongs to one file, and whether a file can be
// committed at all. Pure, so they are tested directly rather than through a
// panel.

export interface GitFile {
  status: string;
  path: string;
}

/** What the tree is, in one word (AC-3). */
export type TreeState = "clean" | "conflict" | "uncommitted";

/// Porcelain's conflict codes. Both columns matter: `UU` is both sides
/// modified, `AU`/`UA`/`DU`/`UD` are added/deleted against unmerged, and
/// `AA`/`DD` are both-added / both-deleted. `git status` reports every one of
/// them as "Unmerged paths", and committing during a merge conflict is how a
/// half-resolved merge reaches a branch.
const CONFLICT_CODES = new Set(["DD", "AU", "UD", "UA", "DU", "AA", "UU"]);

export function isConflict(file: GitFile): boolean {
  return CONFLICT_CODES.has(file.status.trim().length === 2 ? file.status : file.status.padEnd(2));
}

/**
 * The tree's state.
 *
 * Conflict OUTRANKS uncommitted, because they are not alternatives: a
 * conflicted tree is also dirty, and reporting "3 changed" for a tree that is
 * mid-merge tells you the least useful of the two true things.
 */
export function treeState(files: GitFile[]): TreeState {
  if (files.some(isConflict)) return "conflict";
  return files.length > 0 ? "uncommitted" : "clean";
}

/**
 * Split a unified diff into per-file sections, keyed by the path git names in
 * the `+++ b/…` line (falling back to the `diff --git` header for a deletion,
 * where `+++` is `/dev/null`).
 *
 * Parsing the diff we already have, rather than asking the node per file: the
 * status call returns the whole working-tree diff on every poll, so a second
 * round trip per click would fetch bytes we are already holding.
 */
export function splitDiffByFile(diff: string): Record<string, string> {
  const out: Record<string, string> = {};
  if (!diff.trim()) return out;

  let path: string | null = null;
  let lines: string[] = [];
  const flush = () => {
    if (path && lines.length) out[path] = lines.join("\n");
    path = null;
    lines = [];
  };

  for (const line of diff.split("\n")) {
    if (line.startsWith("diff --git ")) {
      flush();
      // `diff --git a/x b/x` — take the b-side, which is the path after any
      // rename. Paths with spaces are why this takes the LAST token rather
      // than splitting on whitespace and indexing.
      const b = line.slice("diff --git ".length);
      const half = b.indexOf(" b/");
      path = half >= 0 ? b.slice(half + 3) : null;
      lines = [line];
      continue;
    }
    if (path === null && !lines.length) continue; // preamble before any file
    lines.push(line);
    if (line.startsWith("+++ b/")) path = line.slice("+++ b/".length);
  }
  flush();
  return out;
}

/**
 * The diff to show for one file, and why there might not be one.
 *
 * An untracked file has no diff at all — `git diff HEAD` does not mention it —
 * so the panel must say "this file is new" rather than render an empty box that
 * reads as "no changes".
 */
export function diffFor(
  file: GitFile,
  sections: Record<string, string>,
): { diff: string } | { reason: string } {
  const found = sections[file.path];
  if (found) return { diff: found };
  if (file.status.includes("?")) {
    return { reason: "Untracked — git has no previous version to compare against." };
  }
  return {
    reason:
      "No diff for this file. It may be a binary file, or the diff may have been truncated because the change is very large.",
  };
}

/**
 * Which files a commit may include.
 *
 * Conflicted files are excluded: staging one marks it resolved, and doing that
 * from a checkbox — with the conflict markers still in the file — is how
 * `<<<<<<< HEAD` reaches a branch. Resolving belongs in the editor.
 */
export function committable(files: GitFile[]): GitFile[] {
  return files.filter((f) => !isConflict(f));
}

/**
 * The `paths` a commit should send: `null` only when the selection IS the whole
 * working tree.
 *
 * `null` means "stage everything" to the node — it runs `git add -A`. So the
 * comparison has to be against EVERY file, not against the committable ones.
 * Comparing against committable was a real defect: during a merge conflict, the
 * conflicted files are unselectable, so "everything selectable is selected"
 * was true with a smaller set, `null` went over the wire, and `git add -A`
 * staged the conflicted files — markers and all — into the commit. The panel
 * says in words that it will not do that; this is what makes it true.
 */
export function commitPaths(files: GitFile[], selected: string[]): string[] | null {
  return selected.length === files.length ? null : selected;
}
