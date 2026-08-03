//! MAIN-102: the MCP notebook tools are person-scoped and route through the
//! notebook module's service paths (validation, encryption, decrypt). Driven at
//! the `NookBackend` layer with a resolved person — the same call the tools make
//! once `mcp_auth` has resolved the caller's identity. Two people see disjoint
//! notebooks; a cross-person read is refused; validation errors propagate.
//! Setup + teardown run through `nook_testkit::TestBed`.

use nook_control::mcp_backend::McpBackend;
use nook_mcp::NookBackend;
use nook_testkit::TestBed;
use nook_types::{CreateUserNote, CreateUserNoteFolder, UpdateUserNote, UpdateUserNoteFolder};

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

/// Folders, nested, moved and deleted through MCP — the gap that made an
/// Obsidian-shaped notebook unusable from a chat client: notes could already be
/// created in folders and moved between them, but nothing could MAKE a folder.
#[tokio::test]
async fn notebook_folders_nest_move_and_delete_through_mcp() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping mcp notebook folder test — no DATABASE_URL");
        return;
    };
    let tenant = bed.tenant("mcpnbf").await;
    let (_u, alice) = bed.user(tenant, "owner").await;
    let backend = McpBackend {
        state: bed.app_state().await,
    };

    let root = backend
        .notebook_create_folder(
            alice,
            CreateUserNoteFolder {
                name: "Projects".into(),
                parent_id: None,
            },
        )
        .await
        .expect("root folder");

    // Nesting: the thing the tree exists for.
    let child = backend
        .notebook_create_folder(
            alice,
            CreateUserNoteFolder {
                name: "NookOS".into(),
                parent_id: Some(root.id),
            },
        )
        .await
        .expect("nested folder");
    assert_eq!(child.parent_id, Some(root.id));

    // A note lands inside the nested folder.
    let note = backend
        .notebook_create_note(
            alice,
            CreateUserNote {
                title: "Design".into(),
                content_md: "# notes".into(),
                folder_id: Some(child.id),
            },
        )
        .await
        .expect("note in a folder");
    assert_eq!(note.folder_id, Some(child.id));

    // Rename and move to the root in one call.
    let moved = backend
        .notebook_update_folder(
            alice,
            child.id,
            UpdateUserNoteFolder {
                name: Some("Nook".into()),
                parent_id: Some(None),
            },
        )
        .await
        .expect("rename + move to root");
    assert_eq!(moved.name, "Nook");
    assert_eq!(moved.parent_id, None, "moving to root did not detach it");

    // Deleting REPARENTS rather than cascading: the note must survive.
    backend
        .notebook_delete_folder(alice, moved.id)
        .await
        .expect("delete the folder");
    let kept = backend
        .notebook_get_note(alice, note.id)
        .await
        .expect("the note outlives its folder");
    assert_eq!(kept.folder_id, None, "the note should rise to the root");

    bed.teardown().await;
}

/// A folder cannot be moved inside its own subtree. The guard lives in the
/// shared service path, so extracting it for MCP rather than reimplementing is
/// what keeps this true on both surfaces — a second copy is a second chance to
/// omit it, and the result would be a subtree detached from the root-anchored
/// path CTE.
#[tokio::test]
async fn a_folder_cannot_be_moved_into_its_own_subtree_through_mcp() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("mcpnbc").await;
    let (_u, alice) = bed.user(tenant, "owner").await;
    let backend = McpBackend {
        state: bed.app_state().await,
    };

    let parent = backend
        .notebook_create_folder(
            alice,
            CreateUserNoteFolder {
                name: "A".into(),
                parent_id: None,
            },
        )
        .await
        .expect("parent");
    let child = backend
        .notebook_create_folder(
            alice,
            CreateUserNoteFolder {
                name: "B".into(),
                parent_id: Some(parent.id),
            },
        )
        .await
        .expect("child");

    assert!(
        backend
            .notebook_update_folder(
                alice,
                parent.id,
                UpdateUserNoteFolder {
                    name: None,
                    parent_id: Some(Some(child.id)),
                },
            )
            .await
            .is_err(),
        "a folder was moved inside its own subtree"
    );

    bed.teardown().await;
}

/// Another person's folder is not yours to nest under, rename or delete — the
/// notebook is private, and the MCP surface must not be the way around that.
#[tokio::test]
async fn one_persons_folders_are_invisible_to_another_through_mcp() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("mcpnbp").await;
    let (_ua, alice) = bed.user(tenant, "owner").await;
    let (_ub, bob) = bed.user(tenant, "owner").await;
    let backend = McpBackend {
        state: bed.app_state().await,
    };

    let hers = backend
        .notebook_create_folder(
            alice,
            CreateUserNoteFolder {
                name: "Private".into(),
                parent_id: None,
            },
        )
        .await
        .expect("alice's folder");

    assert!(
        backend
            .notebook_list_folders(bob)
            .await
            .expect("list")
            .is_empty(),
        "bob can see alice's folders"
    );
    assert!(
        backend
            .notebook_create_folder(
                bob,
                CreateUserNoteFolder {
                    name: "sneak".into(),
                    parent_id: Some(hers.id),
                },
            )
            .await
            .is_err(),
        "bob nested a folder under alice's"
    );
    assert!(
        backend.notebook_delete_folder(bob, hers.id).await.is_err(),
        "bob deleted alice's folder"
    );

    bed.teardown().await;
}
