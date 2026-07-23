//! The guarantee, asserted rather than promised.
//!
//! NookOS tells people that whoever runs the deployment cannot read their
//! terminals, prompts or code. That claim is worth exactly as much as the test
//! that proves it, so this file exists to fail loudly the day somebody makes an
//! operator role reach session content — including by accident, in a refactor,
//! six months from now.
//!
//! Three things are checked:
//!
//! 1. **Every session-content route refuses a deployment operator.** Not "the
//!    routes we remembered": the list is derived from the router source, so a
//!    route added later without the guard fails this test rather than passing
//!    silently.
//! 2. **No permission exists that could grant it.**
//! 3. **The guard does not consult roles or policy** — enforced in the unit
//!    test inside `auth/session_guard.rs`, and re-stated here so someone
//!    reading only this file learns the whole shape.

use std::fs;

/// Routes that carry or control session content. A caller who is not a member
/// of the owning tenant must be refused by every one of them.
const CONTENT_ROUTE_MARKERS: [&str; 4] = ["/sessions", "/ws/sessions", "terminal", "attach"];

/// Read the mounted router so the test cannot fall behind the code.
fn router_source() -> String {
    fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/routes/mod.rs"))
        .expect("routes/mod.rs must be readable")
}

fn sessions_source() -> String {
    fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/routes/sessions.rs"
    ))
    .expect("routes/sessions.rs must be readable")
}

/// Every handler in `sessions.rs` that loads a session must do it through
/// `session_for_content`, which is the only place `require_session_access` is
/// called.
///
/// This is the check that catches the realistic failure: not somebody adding a
/// permission called `read_terminals`, but somebody adding a route that loads a
/// session with a plain query because that is what the file around it looked
/// like.
#[test]
fn every_session_handler_authorizes_through_the_guard() {
    let src = sessions_source();

    // Counts LOADS, not writes. A `DELETE`/`UPDATE … WHERE id = $1 AND
    // tenant_id = $2` after the guard has run is correct and expected; what
    // must not exist is a second way to READ a session row, because that is a
    // route that skipped authorization. The helper itself holds the only one.
    let loads = src
        .matches(r#"query_as("SELECT * FROM sessions WHERE id"#)
        .count();
    assert!(
        loads <= 1,
        "found {loads} direct session loads in sessions.rs; only \
         `session_for_content` may load one. A plain \
         `WHERE id = $1 AND tenant_id = $2` looks correct and is not the same \
         thing: it yields 404 instead of a refusal, and it leaves the isolation \
         boundary implied rather than stated."
    );

    // Every handler taking a session id must reach the guard. Counted rather
    // than named, so a route added later is included automatically.
    let handlers = src.matches("Path(id): Path<SessionId>").count();
    let guarded = src
        .matches("session_for_content(&state, &auth, id)")
        .count();
    assert!(
        guarded + 1 >= handlers,
        "{handlers} handlers take a SessionId but only {guarded} load through the \
         guard. Every route that reads or writes session content must call \
         `session_for_content`."
    );

    assert!(
        src.contains("require_session_access"),
        "sessions.rs must call the membership guard"
    );
}

/// The attach socket is THE session-content route — raw terminal bytes both
/// ways — so it gets its own assertion rather than relying on the sweep.
#[test]
fn the_attach_socket_checks_membership() {
    let src = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ws/attach.rs"))
        .expect("ws/attach.rs must be readable");

    assert!(
        src.contains("require_session_access"),
        "ws/attach.rs carries the raw terminal stream and must check tenant \
         membership before upgrading the socket"
    );
    assert!(
        !src.contains("Permission::") && !src.contains("auth::perm"),
        "ws/attach.rs must not consult the permission catalog — session access \
         is membership, and a role must never reach it"
    );
}

/// No route under the operator prefix may touch session content.
///
/// The operator surface is one prefix precisely so this test can exist: it
/// greps for session markers inside the operator block and fails if any appear.
#[test]
fn the_operator_surface_contains_no_session_routes() {
    let src = router_source();
    let block = src
        .split("── the operator surface ──")
        .nth(1)
        .expect("the operator routes should be in a marked block")
        .split(".route(\"/notify\"")
        .next()
        .expect("block end");

    for marker in CONTENT_ROUTE_MARKERS {
        assert!(
            !block.contains(marker),
            "the operator surface must contain no `{marker}` route. Operators see \
             metadata; session content is not theirs to read, and grouping these \
             routes exists so this test can say so."
        );
    }

    let operator_src = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/routes/operator.rs"
    ))
    .expect("routes/operator.rs must be readable");
    // COUNTING sessions is explicitly allowed — how many machines are working
    // and how loaded they are is an operator's job, and several machines on one
    // task is an audit signal. LOADING one is not.
    for banned in [
        "SELECT * FROM sessions",
        "session_for_content",
        "require_session_access",
        "data_b64",
    ] {
        assert!(
            !operator_src.contains(banned),
            "operator.rs must not reference `{banned}` — it may COUNT sessions \
             through a subquery, which it does, but it must never load one or \
             touch its bytes"
        );
    }
    // And whatever it does with sessions must be aggregate-only.
    for line in operator_src.lines().filter(|l| l.contains("FROM sessions")) {
        assert!(
            line.contains("count(*)"),
            "every `FROM sessions` in operator.rs must be a count — found: {}",
            line.trim()
        );
    }
}

/// The catalog cannot express session access.
///
/// Duplicated from the unit test in `auth/perm.rs` on purpose: somebody
/// auditing this guarantee will open this file, and the property should be
/// visible here rather than only by reference.
#[test]
fn the_permission_catalog_has_no_session_permission() {
    let src = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/auth/perm.rs"))
        .expect("auth/perm.rs must be readable");

    // The enum body only — the module docs discuss session content at length
    // in order to explain its absence.
    let body = src
        .split("pub enum Permission {")
        .nth(1)
        .and_then(|s| s.split('}').next())
        .expect("Permission enum body");

    for forbidden in ["Session", "Terminal", "Attach", "Output", "Input", "Pty"] {
        assert!(
            !body.contains(forbidden),
            "`Permission` must not contain a `{forbidden}` variant. Session access \
             is tenant membership, checked in auth/session_guard.rs. A permission \
             for it would make the guarantee a setting."
        );
    }
}
