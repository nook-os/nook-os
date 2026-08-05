import { describe, expect, it } from "vitest";
import type { Schemas } from "@nookos/api";
import { terminalTarget } from "./newTerminal";

const at = (node: string, status: string, path = `/w/${node}`): Schemas["WorkspaceLocation"] => ({
  node_id: node,
  node_name: node,
  node_status: status,
  path,
  dirty: false,
});

describe("terminalTarget", () => {
  it("opens straight into the only live checkout", () => {
    const t = terminalTarget([at("azul", "online")]);
    expect(t.kind).toBe("one");
    expect(t.location?.node_id).toBe("azul");
  });

  // Picking "the first" out of several machines is how your shell lands on the
  // wrong host. The choice goes back to the human.
  it("refuses to choose between two live checkouts", () => {
    expect(terminalTarget([at("azul", "online"), at("void", "online")]).kind).toBe("choose");
  });

  it("has nowhere to go when the repo is cloned nowhere", () => {
    expect(terminalTarget([]).kind).toBe("none");
  });

  // An offline node would accept the session row and never start it — the
  // failure would surface later as a mystery instead of now as a choice.
  it("ignores offline checkouts", () => {
    const t = terminalTarget([at("azul", "online"), at("void", "offline")]);
    expect(t.kind).toBe("one");
    expect(t.location?.node_id).toBe("azul");
  });

  it("is 'none', not 'one', when every checkout is offline", () => {
    expect(terminalTarget([at("void", "offline")]).kind).toBe("none");
  });
});
