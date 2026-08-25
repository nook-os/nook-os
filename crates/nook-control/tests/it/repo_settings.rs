//! What a `.nook.toml` does to the stored declaration (MAIN-359 AC-3/AC-4/AC-6).
//!
//! The parser's own rules are unit-tested beside it. What needs a database is
//! the half that decides what a given file does to `workspaces.port_requirements`
//! — and in particular the half that decides what it does NOT do. A broken file
//! leaving the stored ports untouched is the property this card turns on: the
//! alternative reads as "this repo binds nothing", which caps the workspace, and
//! a silent cap for a typo is the worst outcome available here.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use nook_control::auth::{AuthCtx, Principal};
use nook_control::services::{port_leases, repo_settings};
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use tower::ServiceExt;

/// What the API and the broker would see for this workspace.
async fn stored(bed: &TestBed, tenant: TenantId, ws: WorkspaceId) -> Option<Vec<PortRequirement>> {
    let w = bed
        .app_state()
        .await
        .workspaces
        .get(tenant, ws)
        .await
        .expect("read")
        .expect("workspace");
    w.port_requirements
        .map(|v| serde_json::from_value(v).expect("stored shape round-trips"))
}

#[tokio::test]
async fn a_declaration_in_the_repo_is_stored_on_the_workspace() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("settings").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    assert!(
        stored(&bed, tenant, ws).await.is_none(),
        "starts undeclared"
    );

    repo_settings::apply(
        &state,
        tenant,
        ws,
        "/w/repo",
        r#"
[[ports]]
name = "web"
env  = "PORT"

[[ports]]
name = "api"
env  = "API_PORT"
required = true
"#,
    )
    .await;

    let got = stored(&bed, tenant, ws).await.expect("declared");
    assert_eq!(
        got.iter().map(|p| p.env.as_str()).collect::<Vec<_>>(),
        vec!["PORT", "API_PORT"]
    );
    assert!(got[1].required);

    // And it is the SAME field the broker reads — one source, not two.
    let effective = port_leases::requirements_of(&state, tenant, Some(ws))
        .await
        .expect("requirements");
    assert_eq!(effective.len(), 2);
    assert_eq!(effective[0].env, "PORT");

    bed.teardown().await;
}

#[tokio::test]
async fn a_later_scan_re_syncs_because_the_repos_answer_wins() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("settings").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    repo_settings::apply(
        &state,
        tenant,
        ws,
        "/w/repo",
        "[[ports]]\nname=\"web\"\nenv=\"PORT\"\n",
    )
    .await;
    // Somebody edits the file and the scan runs again.
    repo_settings::apply(
        &state,
        tenant,
        ws,
        "/w/repo",
        "[[ports]]\nname=\"web\"\nenv=\"WEB_PORT\"\n",
    )
    .await;

    let got = stored(&bed, tenant, ws).await.expect("declared");
    assert_eq!(got.len(), 1, "replaced, not appended");
    assert_eq!(got[0].env, "WEB_PORT");

    bed.teardown().await;
}

#[tokio::test]
async fn a_broken_file_leaves_the_stored_declaration_alone() {
    // AC-4, and the reason it is an AC at all: the tempting failure mode is to
    // treat an unparseable file as an empty declaration, which silently caps the
    // workspace for a missing bracket.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("settings").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    repo_settings::apply(
        &state,
        tenant,
        ws,
        "/w/repo",
        "[[ports]]\nname=\"web\"\nenv=\"PORT\"\n",
    )
    .await;

    for broken in [
        "[[ports]\nname = \"web\"\n", // not TOML
        "[[ports]]\nname=\"a\"\nenv=\"P\"\n[[ports]]\nname=\"a\"\nenv=\"Q\"\n", // duplicate name
        "[[ports]]\nname=\"a\"\nenv=\"P\"\n[[ports]]\nname=\"b\"\nenv=\"P\"\n", // duplicate env
        "[[ports]]\nname=\"a\"\nenv=\"MY PORT\"\n", // unusable env name
    ] {
        repo_settings::apply(&state, tenant, ws, "/w/repo", broken).await;
        let got = stored(&bed, tenant, ws).await.expect("still declared");
        assert_eq!(got.len(), 1, "unchanged after: {broken:?}");
        assert_eq!(got[0].env, "PORT", "unchanged after: {broken:?}");
    }

    bed.teardown().await;
}

#[tokio::test]
async fn an_empty_ports_array_is_a_declaration_and_no_ports_key_is_not() {
    // AC-6. These two are one keystroke apart in the file and mean opposite
    // things: one says "this repo binds nothing" and one says nothing at all.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("settings").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    repo_settings::apply(
        &state,
        tenant,
        ws,
        "/w/repo",
        "[[ports]]\nname=\"web\"\nenv=\"PORT\"\n",
    )
    .await;

    // A file about something else entirely: says nothing about ports, so the
    // declaration stands.
    repo_settings::apply(
        &state,
        tenant,
        ws,
        "/w/repo",
        "[build]\ncommand = \"cargo build\"\n",
    )
    .await;
    assert_eq!(
        stored(&bed, tenant, ws)
            .await
            .expect("still declared")
            .len(),
        1
    );

    // An explicit empty list: this repo binds nothing.
    repo_settings::apply(&state, tenant, ws, "/w/repo", "ports = []\n").await;
    assert_eq!(
        stored(&bed, tenant, ws).await.expect("declared empty"),
        vec![],
        "an empty array is stored as an empty list, not left as before"
    );
    // And the broker honours it rather than falling back to the default.
    assert!(port_leases::requirements_of(&state, tenant, Some(ws))
        .await
        .expect("requirements")
        .is_empty());

    bed.teardown().await;
}

/// The declaration the UI writes, straight into the column the scan also
/// writes — the "somebody marked it in the workspace settings" half of AC-5.
async fn declare(bed: &TestBed, tenant: TenantId, ws: WorkspaceId, reqs: Vec<PortRequirement>) {
    bed.app_state()
        .await
        .workspaces
        .set_port_requirements(tenant, ws, Some(serde_json::to_value(&reqs).unwrap()))
        .await
        .expect("declare");
}

fn listener(name: &str, env: &str) -> PortRequirement {
    PortRequirement {
        name: name.into(),
        env: env.into(),
        protocol: "tcp".into(),
        required: false,
        runtimes: Vec::new(),
        browsable: false,
        path: "/".into(),
    }
}

#[tokio::test]
async fn a_repo_with_two_frontends_resolves_to_two_targets_in_file_order() {
    // MAIN-596 AC-3/AC-7. The resolver is the only definition of the question,
    // so this is the test that a repo with an app and an admin panel gets both
    // — the case a single hardcoded `NOOK_WEB_PORT` could never express.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("browsable").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    repo_settings::apply(
        &state,
        tenant,
        ws,
        "/w/repo",
        r#"
[[ports]]
name = "web"
env  = "WEB_PORT"
browsable = true

[[ports]]
name = "api"
env  = "API_PORT"

[[ports]]
name = "admin"
env  = "ADMIN_PORT"
browsable = true
path = "/admin"
"#,
    )
    .await;

    let targets = port_leases::browsable_targets(&state, tenant, Some(ws))
        .await
        .expect("targets");
    assert_eq!(
        targets,
        vec![
            BrowsableTarget {
                name: "web".into(),
                env: "WEB_PORT".into(),
                path: "/".into()
            },
            BrowsableTarget {
                name: "admin".into(),
                env: "ADMIN_PORT".into(),
                path: "/admin".into()
            },
        ]
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_declaration_that_says_nothing_about_browsing_has_no_targets_and_leases_as_before() {
    // The other half of AC-1: this labels what is already declared (NG-1), so a
    // file written before the field existed must lease exactly what it leased.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("browsable").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let file = "[[ports]]\nname=\"web\"\nenv=\"PORT\"\nrequired=true\n[[ports]]\nname=\"api\"\nenv=\"API_PORT\"\n";
    repo_settings::apply(&state, tenant, ws, "/w/repo", file).await;

    assert!(port_leases::browsable_targets(&state, tenant, Some(ws))
        .await
        .expect("targets")
        .is_empty());

    let effective = port_leases::requirements_of(&state, tenant, Some(ws))
        .await
        .expect("requirements");
    assert_eq!(
        effective
            .iter()
            .map(|r| (r.name.as_str(), r.env.as_str(), r.required))
            .collect::<Vec<_>>(),
        vec![("web", "PORT", true), ("api", "API_PORT", false)]
    );

    // An undeclared workspace is unchanged too: the fallback listener is not a
    // frontend somebody chose.
    let other = bed.workspace(tenant).await;
    assert!(port_leases::browsable_targets(&state, tenant, Some(other))
        .await
        .expect("targets")
        .is_empty());

    bed.teardown().await;
}

#[tokio::test]
async fn the_file_wins_where_it_declares_and_the_workspace_fills_in_where_it_does_not() {
    // AC-5, both directions, because they are one rule and testing either alone
    // passes for the wrong implementation: wholesale replacement satisfies the
    // second sentence and wipes the first, and never syncing satisfies the
    // first and ignores the repo.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("browsable").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    // Somebody marks the app browsable in the settings UI.
    let mut web = listener("web", "WEB_PORT");
    web.browsable = true;
    web.path = "/app".into();
    declare(&bed, tenant, ws, vec![web, listener("api", "API_PORT")]).await;

    // The repo declares its ports and says nothing about browsing. The scan
    // must not read that silence as "no frontend".
    let silent = "[[ports]]\nname=\"web\"\nenv=\"WEB_PORT\"\nrequired=true\n[[ports]]\nname=\"api\"\nenv=\"API_PORT\"\n";
    repo_settings::apply(&state, tenant, ws, "/w/repo", silent).await;
    assert_eq!(
        port_leases::browsable_targets(&state, tenant, Some(ws))
            .await
            .expect("targets"),
        vec![BrowsableTarget {
            name: "web".into(),
            env: "WEB_PORT".into(),
            path: "/app".into()
        }],
        "the UI's answer survives a scan of a file that is silent about it"
    );
    // And the file still won on everything it DID state.
    assert!(
        stored(&bed, tenant, ws).await.expect("declared")[0].required,
        "the file's `required` replaced the stored one"
    );

    // Now the repo states it, and the repo wins — including saying `false`.
    repo_settings::apply(
        &state,
        tenant,
        ws,
        "/w/repo",
        "[[ports]]\nname=\"web\"\nenv=\"WEB_PORT\"\nbrowsable=false\n[[ports]]\nname=\"admin\"\nenv=\"ADMIN_PORT\"\nbrowsable=true\n",
    )
    .await;
    assert_eq!(
        port_leases::browsable_targets(&state, tenant, Some(ws))
            .await
            .expect("targets"),
        vec![BrowsableTarget {
            name: "admin".into(),
            env: "ADMIN_PORT".into(),
            path: "/".into()
        }],
        "an explicit `false` in the file overrides what the UI set"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_browsable_udp_listener_leaves_the_stored_declaration_alone() {
    // AC-8 through the same door AC-4 uses for every other bad declaration: a
    // named error, and the stored ports untouched rather than silently capped.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("browsable").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    declare(&bed, tenant, ws, vec![listener("web", "WEB_PORT")]).await;
    repo_settings::apply(
        &state,
        tenant,
        ws,
        "/w/repo",
        "[[ports]]\nname=\"stream\"\nenv=\"STREAM_PORT\"\nprotocol=\"udp\"\nbrowsable=true\n",
    )
    .await;

    let got = stored(&bed, tenant, ws).await.expect("still declared");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].name, "web");

    bed.teardown().await;
}

/// MAIN-597 AC-3: the resolver reached from OFF the machine.
///
/// A build run records what it built from a node, so it cannot call
/// `browsable_targets` in process — and the alternative it would otherwise take
/// is reading the declaration and filtering it itself, which is the
/// re-derivation MAIN-596 wrote the resolver to prevent. The endpoint is
/// therefore the resolver and nothing else: same order, same paths.
#[tokio::test]
async fn the_browsable_endpoint_answers_exactly_what_the_resolver_does() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("browsable-api").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let mut web = listener("web", "WEB_PORT");
    web.browsable = true;
    let mut admin = listener("admin", "ADMIN_PORT");
    admin.browsable = true;
    admin.path = "/admin".into();
    declare(
        &bed,
        tenant,
        ws,
        vec![web, listener("api", "API_PORT"), admin],
    )
    .await;

    let over_http = nook_control::routes::workspaces::get_browsable_targets(
        axum::extract::State(state.clone()),
        user_ctx(user, tenant),
        axum::extract::Path(ws),
    )
    .await
    .expect("browsable targets")
    .0;
    assert_eq!(
        over_http,
        port_leases::browsable_targets(&state, tenant, Some(ws))
            .await
            .expect("targets"),
    );
    assert_eq!(
        over_http
            .iter()
            .map(|t| (t.name.as_str(), t.env.as_str(), t.path.as_str()))
            .collect::<Vec<_>>(),
        vec![("web", "WEB_PORT", "/"), ("admin", "ADMIN_PORT", "/admin")],
        "declaration order, and the API listener is not a target"
    );

    bed.teardown().await;
}

/// A workspace in another tenant is a 404, not an empty list: "nothing to open
/// here" and "not yours to ask about" must not read the same to a caller
/// deciding whether to record.
#[tokio::test]
async fn the_browsable_endpoint_refuses_another_tenants_workspace() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let mine = bed.tenant("browsable-mine").await;
    let (user, _) = bed.user(mine, "owner").await;
    let theirs = bed.tenant("browsable-theirs").await;
    let ws = bed.workspace(theirs).await;
    let state = bed.app_state().await;

    let refused = nook_control::routes::workspaces::get_browsable_targets(
        axum::extract::State(state.clone()),
        user_ctx(user, mine),
        axum::extract::Path(ws),
    )
    .await;
    assert!(
        refused.is_err(),
        "another tenant's workspace is not visible"
    );

    bed.teardown().await;
}

fn user_ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(uuid::Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

/// The URL the CLI hardcodes, through the real router (MAIN-597 AC-3).
///
/// `nook ports list --browsable` names this path as a string on another
/// machine, so nothing but a request through `build_router` can tell that the
/// route is registered where the client asks for it. The handler tests above
/// would pass just as happily with the route never mounted.
#[tokio::test]
async fn the_browsable_route_is_mounted_where_the_cli_asks_for_it() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("browsable-route").await;
    let (user, _) = bed.user(tenant, "owner").await;
    let ws = bed.workspace(tenant).await;
    let state = bed.app_state().await;

    let mut web = listener("web", "WEB_PORT");
    web.browsable = true;
    web.path = "/app".into();
    declare(&bed, tenant, ws, vec![web]).await;

    bed.db()
        .exec(
            "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
             VALUES ($1, $2, 'user', $3, 'owner')",
            params![uuid::Uuid::new_v4(), tenant, user],
        )
        .await
        .expect("grant");
    let sid = uuid::Uuid::new_v4();
    let expires = nook_db::dialect::time_math(bed.db().engine()).now_plus_scaled("$4", "1 hour");
    bed.db()
        .exec(
            &format!(
                "INSERT INTO sessions_auth (id, user_id, tenant_id, expires_at)
                 VALUES ($1, $2, $3, {expires})"
            ),
            params![sid, user, tenant, 1_i32],
        )
        .await
        .expect("session");

    let resp = nook_control::routes::build_router(state)
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/workspaces/{ws}/browsable"))
                .header(header::COOKIE, format!("nook_session={sid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("the route answers");
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("body");
    let targets: Vec<BrowsableTarget> = serde_json::from_slice(&bytes).expect("the resolver shape");
    assert_eq!(
        targets,
        vec![BrowsableTarget {
            name: "web".into(),
            env: "WEB_PORT".into(),
            path: "/app".into()
        }]
    );

    bed.teardown().await;
}
