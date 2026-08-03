// Where you were, per tenant, so leaving a section and coming back lands you
// where you left rather than at its front page.
//
// Two things used this differently and neither survived navigation. The Board's
// tab lives in `?view=`, which survives a REFRESH but not a trip to another
// section — coming back always dropped you on the kanban even if you had been
// working the backlog. The session tabs had nothing at all: the strip knew
// which tab was active from the route, so landing on the sessions index landed
// you nowhere.
//
// SCOPED BY TENANT, deliberately. A session id from one tenant is meaningless in
// another, and restoring it would either 404 or — worse, if ids ever collide —
// open something from the wrong place. The tenant is part of the key, so
// switching tenants simply has no memory yet rather than the wrong one.
//
// Best-effort by design: every read and write is wrapped, because storage can be
// unavailable (private mode, disabled cookies) and a remembered tab is not worth
// a crashed page.

const PREFIX = "nook.lastPlace.v1";

function key(tenant: string | undefined, what: string): string | null {
  const t = tenant?.trim();
  // No tenant, no memory. Guessing a shared key would leak one tenant's last
  // place into another's, which is the one thing this must not do.
  return t ? `${PREFIX}.${t}.${what}` : null;
}

export function remember(
  tenant: string | undefined,
  what: string,
  value: string,
): void {
  const k = key(tenant, what);
  if (!k) return;
  try {
    window.localStorage.setItem(k, value);
  } catch {
    // Storage unavailable: the choice just will not persist.
  }
}

export function recall(tenant: string | undefined, what: string): string | null {
  const k = key(tenant, what);
  if (!k) return null;
  try {
    return window.localStorage.getItem(k);
  } catch {
    return null;
  }
}

export function forget(tenant: string | undefined, what: string): void {
  const k = key(tenant, what);
  if (!k) return;
  try {
    window.localStorage.removeItem(k);
  } catch {
    // Nothing to do — a stale entry is handled by the caller checking that what
    // it remembers still exists.
  }
}
