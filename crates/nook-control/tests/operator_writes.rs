//! Properties of the operator write surface that must not drift.
//!
//! The read surface got `session_isolation.rs`. Writes need their own, because
//! the failure modes are different: a read that leaks is a privacy failure, a
//! write that is under-authorized is somebody changing infrastructure they do
//! not own.
//!
//! These are source-level assertions rather than integration tests on purpose.
//! The thing worth catching is not "does this endpoint work" — the live checks
//! cover that — but "did somebody add a route that skipped the predicate",
//! which is a property of the code, and which no amount of testing the routes
//! that DO exist would reveal.

use std::fs;

fn operator_src() -> String {
    fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/routes/operator.rs"
    ))
    .expect("routes/operator.rs must be readable")
}

/// Every migration's SQL, concatenated. Reading the directory rather than a
/// filename keeps these assertions alive across renames and squashes.
fn all_migrations() -> String {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
    let mut sql = String::new();
    for entry in fs::read_dir(dir).expect("migrations dir") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|e| e == "sql") {
            sql.push_str(&fs::read_to_string(&path).unwrap_or_default());
            sql.push('\n');
        }
    }
    assert!(!sql.is_empty(), "no migrations found; the scan has broken");
    sql
}

/// Every handler on the operator surface authorizes with a Permission.
///
/// `require_user()` means "is a person", which is not an authorization
/// decision — it was what `nodes::delete` used, and it let any signed-in
/// account delete a node. On this surface that would mean any signed-in account
/// acting on somebody else's deployment.
#[test]
fn every_operator_handler_names_a_permission() {
    let src = operator_src();
    let handlers = src.matches("pub async fn ").count();
    // `require(` for permission checks, plus the CA routes which delegate to
    // `gate_tenant` — itself a `require(CaRotate, …)`.
    let guarded = src.matches(".require(").count() + src.matches("gate_tenant(").count();

    assert!(
        guarded >= handlers,
        "{handlers} handlers in operator.rs but only {guarded} authorization checks. \
         Every operator route must name a Permission and a Scope — `require_user()` \
         is 'is a person', not a permission."
    );
    assert!(
        !src.contains("auth.require_user()?;"),
        "operator.rs must not authorize with require_user alone"
    );
}

/// Writes act on the TARGET's scope, never the caller's.
///
/// `Scope::Tenant(auth.tenant_id)` on this surface would mean an operator could
/// only act on machines in their own tenant — the opposite of the job — and
/// would silently pass for anybody, since everybody holds something in their
/// own tenant.
#[test]
fn writes_scope_to_the_target_not_the_caller() {
    let src = operator_src();
    assert!(
        !src.contains("Scope::Tenant(auth.tenant_id)"),
        "an operator route scoped to the CALLER's tenant authorizes the wrong \
         thing: it passes for anyone acting on themselves and refuses the \
         cross-tenant case this surface exists for"
    );
}

/// Nothing destroys a tenant's work.
///
/// `operator` deliberately does not hold `tenant.manage`. That is only
/// meaningful if no route reaches around it — an operator may stop a machine,
/// and may never delete the work on it.
#[test]
fn no_operator_route_destroys_tenant_data() {
    let src = operator_src();
    for destructive in [
        "DELETE FROM tenants",
        "DELETE FROM workspaces",
        "DELETE FROM tasks",
        "DELETE FROM sessions",
        "DELETE FROM users",
        "DELETE FROM notes",
    ] {
        assert!(
            !src.contains(destructive),
            "operator.rs must not `{destructive}` — operators run the \
             infrastructure, they do not own the work on it. Removing a node is \
             the limit, and it stops a machine rather than deleting what it did."
        );
    }
}

/// The operator surface is not reachable from MCP.
///
/// Decided deliberately: `mcp_auth` accepts a shared static token and
/// `McpBackend` acts as the first user in the first tenant, so it never builds
/// an `AuthCtx` and `require()` cannot run. Operator tools there would be a
/// shared credential carrying deployment-wide authority with misattributed
/// audit. This asserts the exclusion instead of relying on remembering it.
#[test]
fn mcp_exposes_no_operator_tools() {
    let mcp = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../nook-mcp/src/lib.rs"
    ))
    .expect("nook-mcp/src/lib.rs must be readable");

    // Tool names only — the descriptions discuss plenty of things these words
    // appear in.
    let names: Vec<&str> = mcp
        .lines()
        .filter_map(|l| l.trim().strip_prefix("async fn "))
        .filter_map(|l| l.split('(').next())
        .collect();

    for name in &names {
        for forbidden in [
            "rotate", "revoke", "grant", "operator", "binding", "policy", "org_",
        ] {
            assert!(
                !name.contains(forbidden),
                "MCP tool `{name}` looks like an operator action. The operator \
                 surface is REST + CLI + UI only until MCP has a real principal \
                 — see tests/operator_writes.rs for why."
            );
        }
    }
    assert!(
        !names.is_empty(),
        "the tool scan found nothing; it has broken"
    );
}

/// The RBAC.md rule, asserted against the seeded catalog rather than trusted.
///
/// "How CA rotation authority fits: tenant admins never get it." The CA is the
/// deployment's trust root; a tenant able to rotate it is a tenant reaching
/// upward.
#[test]
fn tenant_admin_never_gets_ca_rotation() {
    // Every migration, not one named file: the nineteen were squashed into
    // `0001_init.sql`, and a test pinned to a filename asserts nothing the day
    // that file is renamed — it just stops running.
    let migrations = all_migrations();

    // Every ('tenant_admin', '…') pair in the seed.
    for line in migrations.lines() {
        let l = line.trim();
        if l.starts_with("('tenant_admin'") {
            assert!(
                !l.contains("ca.rotate"),
                "tenant_admin must never hold ca.rotate — found: {l}"
            );
        }
    }

    // And the code must check the permission rather than a role.
    let ca = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/routes/tenant_ca.rs"
    ))
    .expect("tenant_ca.rs must be readable");
    assert!(
        !ca.contains("require_tenant_admin"),
        "tenant_ca.rs must gate on the `ca.rotate` PERMISSION, not on being a \
         tenant admin — the catalog withholds it from them and the code has to \
         agree"
    );
    assert!(
        ca.contains("Permission::CaRotate"),
        "tenant_ca.rs must check Permission::CaRotate"
    );
}

/// Granting is its own authority, not a side effect of managing orgs.
#[test]
fn granting_requires_its_own_permission() {
    let src = operator_src();
    let grant = src
        .split("pub async fn grant(")
        .nth(1)
        .expect("grant handler")
        .split("pub async fn")
        .next()
        .expect("handler body");
    assert!(
        grant.contains("Permission::RbacGrant"),
        "granting a role must require `rbac.grant`. It required `org.manage`, \
         which `operator` does not hold — so the one role a fresh deployment \
         has could not appoint anybody."
    );
}

/// Every permission the operator surface requires is one `operator` holds.
///
/// This catches a class of bug I hit twice: a route gated on a permission the
/// only bootstrapped role does not have. `grant` required `org.manage`, and
/// creating an org required it too — both unreachable for the one role a fresh
/// deployment starts with, and both invisible until somebody clicked the
/// button. The surface must be callable by the role it exists for.
#[test]
fn operator_holds_every_permission_its_own_surface_requires() {
    let src = operator_src();

    // Permissions named by routes in operator.rs, as catalog keys.
    let mut needed: Vec<String> = Vec::new();
    for (variant, key) in [
        ("Permission::OrgView", "org.view"),
        ("Permission::OrgManage", "org.manage"),
        ("Permission::TenantView", "tenant.view"),
        ("Permission::TenantManage", "tenant.manage"),
        ("Permission::NodeView", "node.view"),
        ("Permission::NodeManage", "node.manage"),
        ("Permission::AuditView", "audit.view"),
        ("Permission::CaRotate", "ca.rotate"),
        ("Permission::PolicyView", "policy.view"),
        ("Permission::PolicyManage", "policy.manage"),
        ("Permission::RbacGrant", "rbac.grant"),
    ] {
        if src.contains(variant) {
            needed.push(key.to_string());
        }
    }
    // The CA routes delegate to gate_tenant, which requires ca.rotate.
    if src.contains("gate_tenant(") {
        needed.push("ca.rotate".into());
    }
    assert!(
        !needed.is_empty(),
        "no permissions found; the scan has broken"
    );

    // What the seed actually grants `operator`, across every migration.
    let granted = all_migrations();
    // Collapse whitespace: the seed aligns its columns, so `('operator',  'x')`
    // and `('operator', 'x')` are the same grant and a literal match would fail
    // on formatting rather than on substance.
    let granted: String = granted.split_whitespace().collect::<Vec<_>>().join(" ");

    for key in needed {
        assert!(
            granted.contains(&format!("('operator', '{key}')")),
            "an /operator route requires `{key}`, but no migration grants it to \
             `operator`. The bootstrap grant makes `operator` the only role a \
             fresh deployment has, so a route it cannot call is a route nobody \
             can call."
        );
    }
}
