// A write that does not happen has to say so.
//
// Every call site reads `data` and drops `error`, which is survivable once and
// disastrous in aggregate: the desktop app lost every write it made on macOS
// and not one screen mentioned it. The reporting is central so a new call site
// cannot forget.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// Same reason as apiclient.test.ts: the client captures `Request` and `fetch`
// when it is imported, and builds relative URLs a document would resolve.
const ctl = vi.hoisted(() => {
  const Real = globalThis.Request;
  class BasedRequest extends Real {
    constructor(input: RequestInfo | URL, init?: RequestInit) {
      super(
        typeof input === "string" && input.startsWith("/")
          ? `http://page.example${input}`
          : input,
        init,
      );
    }
  }
  globalThis.Request = BasedRequest as unknown as typeof Request;

  const state: { reply: () => Promise<Response> } = {
    reply: async () => new Response("{}", { status: 200 }),
  };
  globalThis.fetch = (async () => state.reply()) as unknown as typeof fetch;
  return state;
});

import { api, setWriteFailureHandler, type WriteFailure } from "@nookos/api";

let failures: WriteFailure[];

beforeEach(() => {
  failures = [];
  setWriteFailureHandler((f) => failures.push(f));
  ctl.reply = async () => new Response("{}", { status: 200 });
});

afterEach(() => setWriteFailureHandler(null));

describe("write failures", () => {
  it("reports a write the server refused, with what it said", async () => {
    ctl.reply = async () =>
      new Response(JSON.stringify({ message: "workspace not found" }), {
        status: 400,
        headers: { "Content-Type": "application/json" },
      });

    await api.PATCH("/api/v1/tasks/{id}", {
      params: { path: { id: "abc" } },
      body: { workspace_id: "nope" },
    });

    expect(failures).toHaveLength(1);
    expect(failures[0].method).toBe("PATCH");
    expect(failures[0].path).toBe("/api/v1/tasks/abc");
    expect(failures[0].status).toBe(400);
    // The server's own words, not a bare status: "400" tells nobody anything.
    expect(failures[0].message).toBe("workspace not found");
  });

  it("reports a write that never left the machine", async () => {
    // The WebKit bug, and being offline: fetch throws, so there is no status
    // to inspect. A check that only looked at response codes stayed silent
    // through a total write outage.
    ctl.reply = async () => {
      throw new TypeError("ReadableStream uploading is not supported");
    };

    await api
      .PUT("/api/v1/settings/{key}", {
        params: { path: { key: "theme" } },
        body: { value: "deep-teal", scope: "user" },
      })
      .catch(() => undefined);

    expect(failures).toHaveLength(1);
    expect(failures[0].method).toBe("PUT");
    expect(failures[0].status).toBeUndefined();
    expect(failures[0].message).toContain("ReadableStream");
  });

  it("says nothing about a successful write", async () => {
    await api.PUT("/api/v1/settings/{key}", {
      params: { path: { key: "theme" } },
      body: { value: "deep-teal", scope: "user" },
    });
    expect(failures).toEqual([]);
  });

  it("says nothing about a failed read", async () => {
    // Reads have a query layer with error states; a failed write is a thing
    // the person believes they just did.
    ctl.reply = async () => new Response("nope", { status: 500 });
    await api.GET("/api/v1/settings");
    expect(failures).toEqual([]);
  });

  it("stays quiet on 401, which the auth gate already handles", async () => {
    ctl.reply = async () => new Response("", { status: 401 });
    await api.POST("/api/v1/tasks/{id}/claim", {
      params: { path: { id: "abc" } },
      body: {},
    });
    // Being bounced to sign in is the message; a toast on top is noise.
    expect(failures).toEqual([]);
  });
});
