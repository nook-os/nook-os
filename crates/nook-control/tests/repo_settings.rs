//! What a `.nook.toml` does to the stored declaration (MAIN-359 AC-3/AC-4/AC-6).
//!
//! The parser's own rules are unit-tested beside it. What needs a database is
//! the half that decides what a given file does to `workspaces.port_requirements`
//! — and in particular the half that decides what it does NOT do. A broken file
//! leaving the stored ports untouched is the property this card turns on: the
//! alternative reads as "this repo binds nothing", which caps the workspace, and
//! a silent cap for a typo is the worst outcome available here.

use nook_control::services::{port_leases, repo_settings};
use nook_testkit::TestBed;
use nook_types::*;

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
