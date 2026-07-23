
 Design an RBAC, multi-tenancy scope, and visibility-policy model for NookOS.

 **Scope tree.** Introduce an `orgs` layer between deployment and tenant so one shared control plane can serve federated teams. Model authorization as `role_bindings(subject, role, scope_type, scope_id)` with scope types `deployment | org | tenant`, plus `roles`, `permissions`, `role_permissions`. A binding grants its permissions at that scope and all descendants. A self-hosted operator is `operator @ deployment`; a managed team admin is `operator @ org:X` — the same role at a different scope, not a separate concept.

 **A tenant belongs to exactly one org.** Do not model many-to-many — the scope tree must stay a tree so "can X see Y" is a single ancestor check rather than graph traversal. A person contracting for two companies has two tenants.

 ## Two different mechanisms, deliberately not one

 **Hard constraint — session content. Never policy-controlled, never configurable, no enterprise tier.** Operators must never attach to, read, or record a tenant's tmux session, prompts, or code. Enforce structurally: session I/O must not be expressible in the permission catalog at all, must authorize through a separate tenant-membership guard that never consults scope resolution, and must never consult visibility policy. Include negative tests asserting a deployment-scoped operator gets 403 on every session-content route. A promise with a toggle is not a promise — this is the guarantee the open-source product rests on.

 **Policy-controlled — work metadata.** Everything else an operator might see is governed by a per-org visibility policy, because the legitimate need genuinely varies (a regulated bank needs the PR trail; a startup does not want team leads reading branch names).

 - **Always visible to operators, not policy-gated:** nodes (names, status, resources, owning tenant, session counts) — multiple machines working one task is an audit signal; tenant existence and membership counts; control-plane instances and leases; CA health; audit records; PR *existence* (which tenant, when, and review/test verdict).
 - **Policy-gated, default off:** repository URLs and names, branch names, worktree paths, task titles and descriptions, PR titles and repo references.

 ## Policy mechanics the design must specify

 - Policy is **org-scoped and stored as data** (versioned rows with timestamps), not env configuration — "what could my employer see on March 12" must be answerable.
 - **Default closed.** A new org starts at minimum visibility; widening is a deliberate, recorded act.
 - **Additive, never subtractive.** Each policy-gated field is individually opted in; the base operator query selects the minimum column set and policy *adds* columns. Never a filter that strips fields, because a missed filter fails open.
 - The operator read model is its own projection with explicit column lists — never a shared query or `SELECT *` over workspace, session, or task rows.
 - **Policy changes are audited and surfaced to affected users.** A silent widening is the failure mode that turns governance into betrayal.
 - Every user can see their org's current policy in plain language in their own UI ("Your organization's operators can see: … They cannot see: terminal content, prompts, code").
 - Include tests asserting that policy-gated fields are absent from every operator response when policy is off, and that no policy value can expose session content.

 ## Also cover

 - Append-only, idempotent migrations with backfill of existing tenants into a default org (the frozen-`0001` rule applies).
 - How `AuthCtx` resolves bindings, and the single `require(permission, scope)` predicate — one function, one test surface, no `if operator … else if team admin …` branching.
 - The bootstrap path granting the first user `operator @ deployment`, idempotent (only when no deployment-scoped binding exists), recorded in `events`.
 - An `/api/v1/operator/*` read-only surface — one prefix containing zero session-I/O routes, so a dangerous diff is visually obvious in review. Writes (CA rotation, node revocation) come after the read surface is proven.
 - An immutable audit model where operator **reads** are themselves audited — "who looked at whose activity" is a question this deployment model will get asked.
 - How CA rotation authority fits: tenant admins never get it.
 - What must be built now versus deferred until the managed offering exists.

One note on sequencing: the policy machinery is the part that can reasonably lag. The scope tree, the single `require()` predicate, the structural session-content guarantee, and the bootstrap binding are the pieces that are painful to retrofit. Policy can start as "everything gated is simply off" — a hardcoded minimum projection — with the per-org rows added when there's an org that wants more.