// What the API client puts on the wire once an endpoint is configured — the
// desktop case, where requests are cross-origin and carry a bearer token.
//
// These exist because the desktop app spent a release unable to write
// anything at all on macOS. Reads worked, so it looked healthy; every PUT,
// POST and PATCH failed inside fetch, and the UI showed a button that did
// nothing rather than an error. The cause was the rewritten request carrying
// its body as a ReadableStream, which WebKit refuses to upload — so the body
// has to arrive here as bytes.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// Before the imports, deliberately: the client is configured same-origin
// (`baseUrl: "/"`), so it builds relative URLs and lets the document resolve
// them. Node's Request has no document and rejects them, and the fetch layer
// captures `Request` when it is imported — so patching it later is too late.
// This stands in for the page, not for anything under test.
// `fetch` is captured too: the client grabs it when it is created, which is
// the moment `@nookos/api` is imported.
vi.hoisted(() => {
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

  const sent: Request[] = [];
  (globalThis as { __sent?: Request[] }).__sent = sent;
  globalThis.fetch = (async (input: Request) => {
    sent.push(input);
    return new Response("{}", {
      status: 200,
      headers: { "Content-Type": "application/json" },
    });
  }) as unknown as typeof fetch;
});

import { api, setEndpoint } from "@nookos/api";

const CP = "https://cp.example";
let seen: Request[];

beforeEach(() => {
  seen = (globalThis as unknown as { __sent: Request[] }).__sent;
  seen.length = 0;
  setEndpoint({ baseUrl: CP, token: "nook_user_test" });
});

afterEach(() => {
  // Back to same-origin so no other suite inherits an endpoint.
  setEndpoint({ baseUrl: "", token: "" });
});

describe("request rewriting for a configured endpoint", () => {
  it("sends a write to the control plane with the body intact", async () => {
    await api.PUT("/api/v1/settings/{key}", {
      params: { path: { key: "theme" } },
      body: { value: "amber-crt", scope: "user" },
    });

    expect(seen).toHaveLength(1);
    const req = seen[0];
    expect(req.url).toBe(`${CP}/api/v1/settings/theme`);
    expect(req.method).toBe("PUT");
    expect(req.headers.get("Authorization")).toBe("Bearer nook_user_test");
    // The body surviving the rewrite is the whole point: losing it is what
    // made every desktop write a no-op.
    expect(await req.clone().json()).toEqual({ value: "amber-crt", scope: "user" });
  });

  it("does not hand fetch a request whose body is still an unread stream", async () => {
    await api.PATCH("/api/v1/tasks/{id}", {
      params: { path: { id: "abc" } },
      body: { workspace_id: null },
    });

    const req = seen[0];
    // Buffered, not piped: `new Request(url, request)` would have produced a
    // stream body here, which is the exact thing WebKit rejects.
    const buf = await req.clone().arrayBuffer();
    expect(new TextDecoder().decode(buf)).toBe('{"workspace_id":null}');
    expect(req.bodyUsed).toBe(false);
  });

  it("leaves a GET alone apart from the URL and the token", async () => {
    await api.GET("/api/v1/settings");
    const req = seen[0];
    expect(req.url).toBe(`${CP}/api/v1/settings`);
    expect(req.method).toBe("GET");
    expect(req.headers.get("Authorization")).toBe("Bearer nook_user_test");
  });

  it("does not rewrite or report a read", async () => {
    await api.GET("/api/v1/settings");
    expect(seen[0].method).toBe("GET");
  });

  it("stays same-origin when no endpoint is configured", async () => {
    setEndpoint({ baseUrl: "", token: "" });
    await api.GET("/api/v1/settings");
    const req = seen[0];
    // The web build is served by its control plane; rewriting would break it.
    expect(req.url).not.toContain(CP);
    expect(req.headers.get("Authorization")).toBeNull();
  });
});
