// The tenant switcher's shape, kept as a pure function so it can be tested
// without rendering. A person in one tenant sees a plain label; a person in
// several sees a menu with the active one marked and the rest selectable.
import type { MeResponse, TenantMembership } from "@nookos/api";

export interface TenantSwitcherModel {
  /** Render a menu (more than one tenant) vs a plain label (exactly one). */
  isMenu: boolean;
  /** The active tenant's name, always shown as the trigger/label. */
  currentName: string;
  /** The active tenant's id, or null if somehow unknown. */
  currentId: string | null;
  /** Every tenant, active first, then the rest by name — the menu order. */
  options: TenantMembership[];
}

/** Derive the switcher model from `/auth/me`. Falls back to the singular
 *  `me.tenant` when the memberships list has not loaded, so the label is never
 *  blank. */
export function tenantSwitcherModel(me: MeResponse): TenantSwitcherModel {
  const tenants = me.tenants ?? [];
  const current =
    tenants.find((t) => t.current) ??
    // Before the list arrives (or an older server), synthesize a one-element
    // membership from the singular tenant so the label still renders.
    ({
      id: me.tenant.id,
      name: me.tenant.name,
      slug: me.tenant.slug,
      role: me.user.role,
      current: true,
      created_at: me.tenant.created_at,
    } as TenantMembership);

  const rest = tenants
    .filter((t) => t.id !== current.id)
    .sort((a, b) => a.name.localeCompare(b.name));

  return {
    isMenu: tenants.length > 1,
    currentName: current.name,
    currentId: current.id ?? null,
    options: [current, ...rest],
  };
}
