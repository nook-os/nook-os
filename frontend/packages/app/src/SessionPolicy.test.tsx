// The session policy editor, and the projection it now shows before you commit
// to anything (MAIN-500).
//
// The assertions that matter are about WHICH spec was asked about and about
// what the answer is allowed to claim. A preview that quietly answers for the
// SAVED spec looks perfectly plausible on screen — it is a real answer, from
// the real planner, about the wrong question — and only the request body can
// tell the difference. The same goes for staleness: an answer to the previous
// keystroke renders exactly like an answer to this one.
import React from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

type Blocker = Record<string, unknown>;

const SPEC = {
  runtime: "claude",
  node_selector: {} as Record<string, string>,
  tolerations: [] as { key: string; effect: string }[],
  replicas: { kind: "single" as const },
};

/** A preview answer, with only the fields the panel reads. */
function preview(over: Record<string, unknown> = {}) {
  return {
    matched: [{ node_id: "n-1", node_name: "alpha" }],
    needs_clone: [] as Blocker[],
    ineligible: [] as Blocker[],
    desired: 1,
    placed: 1,
    shortfall: 0,
    capped: false,
    ...over,
  };
}

const state = vi.hoisted(() => ({
  spec: null as unknown,
  status: null as unknown,
  preview: null as unknown,
  previewFails: false,
}));

/** Every preview request's body — the assertion surface for "which spec". */
const previews = vi.hoisted(() => [] as Record<string, unknown>[]);
const put = vi.hoisted(() => vi.fn());

vi.mock("@nookos/api", () => ({
  api: {
    GET: vi.fn(async (path: string) => {
      if (path.includes("reconcile-status")) return { data: state.status };
      if (path.includes("session-spec")) return { data: state.spec };
      return { data: null };
    }),
    POST: vi.fn(async (path: string, opts?: { body?: Record<string, unknown> }) => {
      if (!path.includes("reconcile-preview")) return { data: null };
      previews.push(opts?.body ?? {});
      if (state.previewFails) return { error: { error: "no" } };
      return { data: state.preview };
    }),
    PUT: put,
  },
}));

vi.mock("./dialogs", () => ({ notify: vi.fn(async () => undefined) }));

import { blockerParts, draftSpec, SessionPolicy } from "./SessionPolicy";

function mount() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <SessionPolicy workspaceId="ws-1" />
    </QueryClientProvider>,
  );
}

/** Open the editor, seeded from the saved spec. */
async function openEditor(user: ReturnType<typeof userEvent.setup>) {
  const button = await screen.findByRole("button", { name: /edit|declare/ });
  await user.click(button);
  await screen.findByTestId("policy-region-what");
}

/** The spec the last preview asked about. */
const lastAsked = () =>
  previews.length
    ? (previews[previews.length - 1].spec as typeof SPEC)
    : undefined;

beforeEach(() => {
  cleanup();
  previews.length = 0;
  state.spec = { ...SPEC };
  state.status = {
    enabled: true,
    managed: true,
    desired: 1,
    running: 1,
    shortfall: 0,
    port_capped: false,
    blocked: [],
    eligible: 1,
  };
  state.preview = preview();
  state.previewFails = false;
  put.mockReset();
  put.mockImplementation(async () => ({ data: null }));
});

describe("the preview asks about the draft", () => {
  it("sends the UNSAVED values, and saves nothing to get them (AC-1)", async () => {
    const user = userEvent.setup();
    mount();
    await openEditor(user);

    await user.selectOptions(screen.getByRole("combobox"), "count");
    const count = screen.getByRole("spinbutton");
    await user.clear(count);
    await user.type(count, "3");

    await waitFor(() =>
      expect(lastAsked()?.replicas).toEqual({ kind: "count", count: 3 }),
    );
    // The saved spec is still `single`, and nothing was written to find out
    // what `count: 3` would do.
    expect(put).not.toHaveBeenCalled();
  });

  it("re-asks as the draft changes (AC-2)", async () => {
    const user = userEvent.setup();
    mount();
    await openEditor(user);
    await waitFor(() => expect(previews.length).toBeGreaterThan(0));

    const runtime = screen.getByDisplayValue("claude");
    await user.clear(runtime);
    await user.type(runtime, "codex");

    await waitFor(() => expect(lastAsked()?.runtime).toBe("codex"));
  });

  it("drops half-typed selector rows, exactly as save would (AC-1)", async () => {
    const user = userEvent.setup();
    mount();
    await openEditor(user);

    const where = screen.getByTestId("policy-region-where");
    await user.click(within(where).getAllByRole("button", { name: /add/ })[0]);
    await user.type(within(where).getByPlaceholderText("os"), "arch");

    // A key with no value matches nothing and the server refuses it; the
    // preview must not be asking about a spec that cannot be saved.
    await waitFor(() => expect(previews.length).toBeGreaterThan(0));
    expect(lastAsked()?.node_selector).toEqual({});
  });

  it("marks an answer that is behind the draft as stale (AC-2)", async () => {
    const user = userEvent.setup();
    mount();
    await openEditor(user);
    await waitFor(() =>
      expect(screen.getByTestId("policy-preview").getAttribute("data-stale")).toBe("false"),
    );

    const runtime = screen.getByDisplayValue("claude");
    await user.type(runtime, "x");
    expect(screen.getByTestId("policy-preview").getAttribute("data-stale")).toBe("true");
    expect(screen.getByTestId("policy-preview-stale")).toBeTruthy();

    await waitFor(() =>
      expect(screen.getByTestId("policy-preview").getAttribute("data-stale")).toBe("false"),
    );
  });

  it("says what would run and where, never as current state (AC-1, AC-2)", async () => {
    state.preview = preview({
      matched: [
        { node_id: "n-1", node_name: "alpha" },
        { node_id: "n-2", node_name: "beta" },
      ],
      desired: 3,
      placed: 2,
      shortfall: 1,
    });
    const user = userEvent.setup();
    mount();
    await openEditor(user);

    const panel = await screen.findByTestId("policy-preview");
    expect(panel.textContent).toContain("if you save this");
    expect(panel.textContent).toContain("projection, nothing has changed yet");
    await waitFor(() =>
      expect(screen.getByTestId("policy-preview-counts").textContent).toContain("2/3"),
    );
    expect(screen.getAllByTestId("policy-preview-node").map((n) => n.textContent)).toEqual([
      "alpha",
      "beta",
    ]);
  });
});

describe("a preview is an aid, not a gate", () => {
  it("says it is unavailable and still saves (AC-3)", async () => {
    state.previewFails = true;
    const user = userEvent.setup();
    mount();
    await openEditor(user);

    await screen.findByTestId("policy-preview-unavailable");
    expect(screen.queryByTestId("policy-preview")).toBeNull();

    const save = screen.getByRole("button", { name: "save policy" });
    expect((save as HTMLButtonElement).disabled).toBe(false);
    await user.click(save);
    await waitFor(() => expect(put).toHaveBeenCalledTimes(1));
    expect(put.mock.calls[0][1].body.spec.runtime).toBe("claude");
  });
});

describe("blocked nodes get a row each", () => {
  const VARIANTS: { reason: Blocker; ground: string; detail: string }[] = [
    { reason: { kind: "offline" }, ground: "offline", detail: "not connected" },
    {
      reason: { kind: "runtime_unavailable", wanted: "claude", available: ["bash"] },
      ground: "no claude runtime",
      detail: "has bash",
    },
    {
      reason: { kind: "selector_mismatch", key: "os", wanted: "linux", actual: "macos" },
      ground: "os mismatch",
      detail: "wants linux, has macos",
    },
    {
      reason: { kind: "untolerated_taint", key: "gpu", effect: "NoSchedule" },
      ground: "untolerated taint",
      detail: "gpu:NoSchedule is not tolerated",
    },
    {
      reason: { kind: "needs_clone" },
      ground: "needs a clone",
      detail: "eligible once the checkout lands",
    },
  ];

  it("renders every NodeBlocker variant with its own detail (AC-5)", () => {
    for (const v of VARIANTS) {
      expect(blockerParts(v.reason as never)).toEqual({
        ground: v.ground,
        detail: v.detail,
      });
    }
  });

  it("distinguishes a missing label from a different one (AC-5)", () => {
    expect(
      blockerParts({ kind: "selector_mismatch", key: "os", wanted: "linux", actual: null } as never)
        .detail,
    ).toBe("wants linux, has no os label");
    expect(
      blockerParts({ kind: "runtime_unavailable", wanted: "claude", available: [] } as never).detail,
    ).toBe("reported no runtimes");
  });

  it("gives each excluded node a row in the preview, with every ground (AC-5)", async () => {
    state.preview = preview({
      matched: [],
      needs_clone: [{ node_id: "n-3", node_name: "gamma", reason: { kind: "needs_clone" } }],
      ineligible: [
        {
          node_id: "n-1",
          node_name: "alpha",
          reasons: [VARIANTS[0].reason, VARIANTS[3].reason],
        },
        { node_id: "n-2", node_name: "beta", reasons: [VARIANTS[2].reason] },
      ],
      desired: 3,
      placed: 0,
      shortfall: 3,
    });
    const user = userEvent.setup();
    mount();
    await openEditor(user);

    const rows = await screen.findAllByTestId("policy-blocked-row");
    expect(rows).toHaveLength(3);
    expect(rows[0].textContent).toContain("gamma");
    expect(rows[1].textContent).toContain("alpha");
    // Both grounds, on the one node's row — not whichever was checked first.
    expect(rows[1].textContent).toContain("offline");
    expect(rows[1].textContent).toContain("gpu:NoSchedule is not tolerated");
    expect(rows[2].textContent).toContain("wants linux, has macos");
  });

  it("lists the saved policy's shortfall a node per row, not a sentence (AC-5)", async () => {
    state.status = {
      enabled: true,
      managed: true,
      desired: 3,
      running: 1,
      shortfall: 2,
      port_capped: false,
      blocked: [
        { node_id: "n-1", node_name: "alpha", reason: { kind: "needs_clone" } },
        {
          node_id: "n-2",
          node_name: "beta",
          reason: { kind: "runtime_unavailable", wanted: "claude", available: ["bash"] },
        },
      ],
      eligible: 3,
    };
    mount();

    const detail = await screen.findByTestId("policy-shortfall-detail");
    expect(detail.textContent).toContain("1/3 running");
    const rows = within(detail).getAllByTestId("policy-blocked-row");
    expect(rows).toHaveLength(2);
    expect(rows[0].textContent).toContain("alpha");
    expect(rows[1].textContent).toContain("has bash");
  });

  it("blames the fleet's size when nothing is blocked (AC-5)", async () => {
    state.status = {
      enabled: true,
      managed: true,
      desired: 5,
      running: 2,
      shortfall: 3,
      port_capped: false,
      blocked: [],
      eligible: 2,
    };
    mount();

    const detail = await screen.findByTestId("policy-shortfall-detail");
    expect(detail.textContent).toContain("2 nodes match");
    expect(within(detail).queryAllByTestId("policy-blocked-row")).toHaveLength(0);
  });
});

describe("the editor still edits", () => {
  it("keeps every control, grouped rather than removed (AC-4, AC-6)", async () => {
    const user = userEvent.setup();
    mount();
    await openEditor(user);

    for (const region of ["what", "how-many", "where"]) {
      expect(screen.getByTestId(`policy-region-${region}`)).toBeTruthy();
    }

    const runtime = screen.getByDisplayValue("claude");
    await user.clear(runtime);
    await user.type(runtime, "codex");
    await user.selectOptions(screen.getByRole("combobox"), "count");
    const count = screen.getByRole("spinbutton");
    await user.clear(count);
    await user.type(count, "2");

    const where = screen.getByTestId("policy-region-where");
    const [addSelector, addToleration] = within(where).getAllByRole("button", { name: /add/ });
    await user.click(addSelector);
    await user.type(within(where).getByPlaceholderText("os"), "os");
    await user.type(within(where).getByPlaceholderText("linux"), "linux");
    await user.click(addToleration);
    await user.type(within(where).getByPlaceholderText("key"), "gpu");

    await user.click(screen.getByRole("button", { name: "save policy" }));
    await waitFor(() => expect(put).toHaveBeenCalledTimes(1));
    // The same PUT body this editor always sent (AC-6): an effect defaulted to
    // NoSchedule, blank rows dropped, the spec shape unchanged.
    expect(put.mock.calls[0][1].body).toEqual({
      spec: {
        runtime: "codex",
        node_selector: { os: "linux" },
        tolerations: [{ key: "gpu", effect: "NoSchedule" }],
        replicas: { kind: "count", count: 2 },
      },
    });
  });

  it("removes a selector row (AC-6)", async () => {
    state.spec = { ...SPEC, node_selector: { os: "linux" } };
    const user = userEvent.setup();
    mount();
    await openEditor(user);

    const where = screen.getByTestId("policy-region-where");
    await user.click(within(where).getAllByRole("button", { name: "remove" })[0]);
    await user.click(screen.getByRole("button", { name: "save policy" }));
    await waitFor(() => expect(put).toHaveBeenCalledTimes(1));
    expect(put.mock.calls[0][1].body.spec.node_selector).toEqual({});
  });

  it("clears the policy from the editor (AC-6)", async () => {
    const user = userEvent.setup();
    mount();
    await openEditor(user);
    await user.click(screen.getByRole("button", { name: "clear" }));
    await waitFor(() => expect(put).toHaveBeenCalledTimes(1));
    expect(put.mock.calls[0][1].body).toEqual({ spec: null });
  });

  it("asks nothing while the editor is closed", async () => {
    mount();
    await screen.findByRole("button", { name: "edit" });
    await new Promise((r) => setTimeout(r, 400));
    expect(previews).toHaveLength(0);
  });
});

describe("draftSpec", () => {
  it("is what save sends: trimmed, blank rows dropped, effect defaulted", () => {
    expect(
      draftSpec(
        { ...SPEC, runtime: "claude" },
        [
          { k: " os ", v: " linux " },
          { k: "arch", v: "" },
          { k: "", v: "" },
        ],
        [
          { k: " gpu ", v: "" },
          { k: "", v: "NoSchedule" },
        ],
      ),
    ).toEqual({
      runtime: "claude",
      node_selector: { os: "linux" },
      tolerations: [{ key: "gpu", effect: "NoSchedule" }],
      replicas: { kind: "single" },
    });
  });
});
