// MAIN-450: the browser-tab title.
//
// The resolver is pure, so the whole AC-2 map is a table here rather than
// twenty renders. The component tests cover the two things a table cannot: that
// `document.title` actually changes on client-side navigation, and that the
// attention dot appears from the caches the shell fills rather than from a
// fetch of its own.
import React from "react";
import { afterEach, describe, expect, it } from "vitest";
import { act, cleanup, render } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter, Route, Routes, useNavigate } from "react-router-dom";

import { BRAND, DocumentTitle, resolveTitle } from "./documentTitle";
import { PENDING_KEY } from "./Interactions";
import { NOTIFICATIONS_KEY } from "./Notifications";

afterEach(cleanup);

/** Signed in, nothing waiting — the ordinary case for every row of the map. */
const titleAt = (pathname: string) => resolveTitle(pathname, true, false);

describe("resolveTitle — the AC-2 map", () => {
  // One row per path the map names, spelled out rather than generated: the
  // point of the table is that a reader can check it against the ticket.
  const ROWS: ReadonlyArray<readonly [string, string]> = [
    ["/mission", "Mission Control · nook@os"],
    ["/workspaces", "Workspaces · nook@os"],
    ["/workspaces/019f840f-2d80-7163-b4b1-8b1e12d7e0d3", "Workspaces · nook@os"],
    ["/sessions", "Sessions · nook@os"],
    ["/sessions/list", "Sessions · nook@os"],
    ["/sessions/s1", "Sessions · nook@os"],
    ["/board", "Board · nook@os"],
    ["/loop/MAIN-450", "Loop · nook@os"],
    ["/chat", "Chat · nook@os"],
    ["/nodes", "Nodes · nook@os"],
    ["/nodes/n1", "Nodes · nook@os"],
    ["/notebook", "Notes · nook@os"],
    ["/admin", "Admin · nook@os"],
    ["/operator", "Admin · nook@os"],
    ["/settings", "Settings · nook@os"],
    ["/team", "Team · nook@os"],
    ["/feedback", "Feedback · nook@os"],
    ["/help", "Docs · nook@os"],
    ["/verify-email", "Verify email · nook@os"],
    ["/accept", "Accept invite · nook@os"],
  ];

  it.each(ROWS)("%s titles as %s", (pathname, expected) => {
    expect(titleAt(pathname)).toBe(expected);
  });

  // The separator is a middle dot with a space each side, and getting it wrong
  // is invisible in a screenshot and obvious in a tab strip.
  it("joins with U+00B7 and one space on each side", () => {
    expect(titleAt("/board")).toBe(`Board \u00b7 ${BRAND}`);
  });

  it("names a detail route after its SECTION, never its entity (NG-1)", () => {
    // The id is in the path and must not be in the title: a tab strip truncates,
    // and "Sessions" is the part that tells two tabs apart.
    expect(titleAt("/sessions/s1")).not.toContain("s1");
    expect(titleAt("/workspaces/w1")).not.toContain("w1");
    expect(titleAt("/nodes/n1")).not.toContain("n1");
    expect(titleAt("/loop/MAIN-450")).not.toContain("MAIN-450");
  });
});

describe("resolveTitle — AC-3, the paths with no section", () => {
  it("titles the dashboard as the bare brand", () => {
    expect(titleAt("/")).toBe("nook@os");
  });

  it("titles an unrouted path as the bare brand, not the section it resembles", () => {
    // `matchPath` and not a prefix test: `/board/nonsense` is a 404, and calling
    // it Board would have the tab claim to be somewhere it is not.
    expect(titleAt("/does-not-exist")).toBe("nook@os");
    expect(titleAt("/board/nonsense")).toBe("nook@os");
    expect(titleAt("/sessions/s1/extra")).toBe("nook@os");
  });

  it("is never blank", () => {
    for (const p of ["/", "", "/x", "/a/b/c/d"]) {
      expect(resolveTitle(p, true, false)).not.toBe("");
    }
  });
});

describe("resolveTitle — AC-4, signed out", () => {
  it("titles the invite landing as Accept invite", () => {
    expect(resolveTitle("/accept", false, false)).toBe("Accept invite · nook@os");
  });

  it("titles every other path as Sign in, because the catch-all IS the login", () => {
    for (const p of ["/", "/board", "/sessions/s1", "/does-not-exist"]) {
      expect(resolveTitle(p, false, false)).toBe("Sign in · nook@os");
    }
  });
});

describe("resolveTitle — AC-5, the attention dot", () => {
  it("prefixes U+25CF and one space", () => {
    expect(resolveTitle("/board", true, true)).toBe("● Board · nook@os");
    expect(resolveTitle("/", true, true)).toBe("● nook@os");
  });

  it("leaves no residual space when there is nothing waiting", () => {
    expect(resolveTitle("/board", true, false)).toBe("Board · nook@os");
    expect(resolveTitle("/board", true, false).startsWith(" ")).toBe(false);
    expect(resolveTitle("/", true, false)).toBe("nook@os");
  });

  it("never shows a number (NG-2)", () => {
    expect(resolveTitle("/board", true, true)).not.toMatch(/\d/);
  });
});

/// Mount the component with a seeded cache, at `path`. `me` is what
/// `["me"]` holds: an object for signed in, `null` for signed out, and absent
/// for "not answered yet".
function mount(
  path: string,
  opts: { me?: unknown; pending?: unknown[]; unread?: number } = {},
) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  if ("me" in opts) qc.setQueryData(["me"], opts.me);
  if (opts.pending) qc.setQueryData(PENDING_KEY, opts.pending);
  if (opts.unread !== undefined) {
    qc.setQueryData(NOTIFICATIONS_KEY, { unread: opts.unread, notifications: [] });
  }
  const result = render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={[path]}>
        <DocumentTitle />
        <Routes>
          <Route path="*" element={<Nav />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
  return { qc, ...result };
}

/** A button per destination, so a test can navigate the way a click does. */
function Nav() {
  const navigate = useNavigate();
  return (
    <>
      <button onClick={() => navigate("/board")}>go board</button>
      <button onClick={() => navigate("/sessions")}>go sessions</button>
    </>
  );
}

describe("<DocumentTitle>", () => {
  it("sets the title from the route it mounted at", () => {
    mount("/board", { me: { user: { id: "u1" } } });
    expect(document.title).toBe("Board · nook@os");
  });

  it("retitles on client-side navigation, twice (AC-6)", () => {
    const { getByText } = mount("/", { me: { user: { id: "u1" } } });
    expect(document.title).toBe("nook@os");

    act(() => getByText("go board").click());
    expect(document.title).toBe("Board · nook@os");

    act(() => getByText("go sessions").click());
    expect(document.title).toBe("Sessions · nook@os");
  });

  it("shows the dot when a pending interaction arrives, without navigating (AC-6)", () => {
    const { qc } = mount("/board", { me: { user: { id: "u1" } }, pending: [] });
    expect(document.title).toBe("Board · nook@os");

    // What a websocket push does: it writes the cache, and nothing navigates.
    act(() => {
      qc.setQueryData(PENDING_KEY, [{ id: "ixn-1" }]);
    });
    expect(document.title).toBe("● Board · nook@os");

    act(() => {
      qc.setQueryData(PENDING_KEY, []);
    });
    expect(document.title).toBe("Board · nook@os");
  });

  it("shows the dot for an unread notification too", () => {
    const { qc } = mount("/board", { me: { user: { id: "u1" } }, unread: 3 });
    expect(document.title).toBe("● Board · nook@os");

    act(() => {
      qc.setQueryData(NOTIFICATIONS_KEY, { unread: 0, notifications: [] });
    });
    expect(document.title).toBe("Board · nook@os");
  });

  it("titles the signed-out app from the same mount (AC-4)", () => {
    mount("/board", { me: null });
    expect(document.title).toBe("Sign in · nook@os");
    cleanup();
    mount("/accept", { me: null });
    expect(document.title).toBe("Accept invite · nook@os");
  });

  it("leaves the document's own title alone until auth answers", () => {
    // A signed-in reload must not flash "Sign in": `index.html` already says
    // the brand, and that is what the tab should hold until we know.
    document.title = BRAND;
    mount("/board");
    expect(document.title).toBe(BRAND);
  });

  it("reads the caches and issues no query of its own (AC-7)", () => {
    const { qc } = mount("/board", { me: { user: { id: "u1" } }, pending: [{ id: "x" }] });
    expect(document.title).toBe("● Board · nook@os");
    // Only the two entries the test seeded. A `useQuery` in the title would
    // have added a third — and, signed out, authenticated fetches with it.
    expect(qc.getQueryCache().getAll()).toHaveLength(2);
  });
});
