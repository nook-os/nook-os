//! MAIN-102: the MCP notebook tools are person-scoped and route through the
//! notebook module's service paths (validation, encryption, decrypt). Driven at
//! the `NookBackend` layer with a resolved person — the same call the tools make
//! once `mcp_auth` has resolved the caller's identity. Two people see disjoint
//! notebooks; a cross-person read is refused; validation errors propagate.
//! Setup + teardown run through `nook_testkit::TestBed`.

use nook_control::mcp_backend::McpBackend;
use nook_mcp::NookBackend;
use nook_testkit::TestBed;
use nook_types::{CreateUserNote, UpdateUserNote};

#[tokio::test]
async fn notebook_is_person_scoped_and_round_trips() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping mcp notebook test — no DATABASE_URL");
        return;
    };
    let tenant = bed.tenant("mcpnb").await;
    // `bed.user` returns (user_id, person_id) — the person is what the notebook
    // scopes on, and what the resolved MCP caller carries.
    let (_ua, alice) = bed.user(tenant, "owner").await;
    let (_ub, bob) = bed.user(tenant, "owner").await;
    let backend = McpBackend {
        state: bed.app_state().await,
    };

    // Alice creates a note; the body round-trips through encrypt → decrypt.
    let note = backend
        .notebook_create_note(
            alice,
            CreateUserNote {
                title: "Alice secret".into(),
                content_md: "the launch codes".into(),
                folder_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(note.title, "Alice secret");
    assert_eq!(
        note.content_md.as_deref(),
        Some("the launch codes"),
        "decrypted body round-trips through the service path (AC-4)"
    );

    // Alice sees exactly her note; Bob's notebook is disjoint (AC-5).
    let alice_notes = backend.notebook_list_notes(alice, None).await.unwrap();
    assert_eq!(alice_notes.len(), 1);
    assert_eq!(alice_notes[0].title, "Alice secret");
    let bob_notes = backend.notebook_list_notes(bob, None).await.unwrap();
    assert!(
        bob_notes.is_empty(),
        "a second person sees none of the first's notes"
    );

    // Bob cannot read Alice's note by id — person scoping on get, not just list.
    assert!(
        backend.notebook_get_note(bob, note.id).await.is_err(),
        "a cross-person get is refused"
    );

    // Owner get / update / delete round-trip.
    let got = backend.notebook_get_note(alice, note.id).await.unwrap();
    assert_eq!(got.content_md.as_deref(), Some("the launch codes"));

    backend
        .notebook_update_note(
            alice,
            note.id,
            UpdateUserNote {
                title: Some("Alice notes".into()),
                content_md: Some("moved on".into()),
                folder_id: None,
            },
        )
        .await
        .unwrap();
    let updated = backend.notebook_get_note(alice, note.id).await.unwrap();
    assert_eq!(updated.title, "Alice notes");
    assert_eq!(updated.content_md.as_deref(), Some("moved on"));

    backend.notebook_delete_note(alice, note.id).await.unwrap();
    assert!(
        backend.notebook_get_note(alice, note.id).await.is_err(),
        "a deleted note is gone"
    );

    // Folders list is person-scoped and starts empty.
    assert!(backend
        .notebook_list_folders(alice)
        .await
        .unwrap()
        .is_empty());

    bed.teardown().await;
}

#[tokio::test]
async fn notebook_create_propagates_validation_errors() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("mcpnbval").await;
    let (_u, person) = bed.user(tenant, "owner").await;
    let backend = McpBackend {
        state: bed.app_state().await,
    };

    // A blank title is the MAIN-84 rule — it must surface as an error the tool
    // relays, not a silent success (AC-3).
    let err = backend
        .notebook_create_note(
            person,
            CreateUserNote {
                title: "   ".into(),
                content_md: "x".into(),
                folder_id: None,
            },
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("blank"),
        "the MAIN-84 blank-title message propagates: {err}"
    );

    bed.teardown().await;
}
