import { describe, expect, it } from "vitest";
import type { MeResponse, TenantMembership } from "@nookos/api";
import { tenantSwitcherModel } from "./tenants";

const tenant = (id: string, name: string, current: boolean): TenantMembership => ({
  id,
  name,
  slug: name.toLowerCase(),
  role: "member",
  current,
  created_at: "2026-01-01T00:00:00Z",
});

const me = (tenants: TenantMembership[]): MeResponse => ({
  user: {
    id: "u1",
    tenant_id: tenants.find((t) => t.current)?.id ?? "t1",
    display_name: "Dev",
    email: "dev@example.com",
    avatar_url: null,
    role: "owner",
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
  },
  tenant: {
    id: tenants.find((t) => t.current)?.id ?? "t1",
    name: tenants.find((t) => t.current)?.name ?? "Personal",
    slug: "personal",
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
  },
  tenants,
  capability: { operator: false, deployment: [], org_id: null },
});

describe("tenantSwitcherModel", () => {
  it("shows a plain label (no menu) for exactly one tenant", () => {
    const m = tenantSwitcherModel(me([tenant("t1", "Personal", true)]));
    expect(m.isMenu).toBe(false);
    expect(m.currentName).toBe("Personal");
    expect(m.currentId).toBe("t1");
  });

  it("shows a menu for more than one tenant, active first then the rest by name", () => {
    const m = tenantSwitcherModel(
      me([
        tenant("t1", "Personal", false),
        tenant("t2", "Zebra Team", true),
        tenant("t3", "Acme", false),
      ]),
    );
    expect(m.isMenu).toBe(true);
    expect(m.currentName).toBe("Zebra Team");
    // Active tenant leads; the rest are alphabetical (Acme before Personal).
    expect(m.options.map((t) => t.name)).toEqual(["Zebra Team", "Acme", "Personal"]);
  });

  it("falls back to the singular tenant when the list has not loaded", () => {
    // An older server (or a response before /me/tenants lands) leaves the list
    // empty; the label must still render from me.tenant rather than going blank.
    const base = me([tenant("t9", "Solo", true)]);
    const m = tenantSwitcherModel({ ...base, tenants: [] });
    expect(m.isMenu).toBe(false);
    expect(m.currentName).toBe("Solo");
    expect(m.currentId).toBe("t9");
  });
});
