//! MAIN-102: the MCP notebook tools are person-scoped and route through the
//! notebook module's service paths (validation, encryption, decrypt). Driven at
//! the `NookBackend` layer with a resolved person — the same call the tools make
//! once `mcp_auth` has resolved the caller's identity. Two people see disjoint
//! notebooks; a cross-person read is refused; validation errors propagate.
//! Setup + teardown run through `nook_testkit::TestBed`.

use axum::extract::{Path, State};
use axum::Json;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::crypto;
use nook_control::mcp_backend::McpBackend;
use nook_control::routes::notebook;
use nook_mcp::NookBackend;
use nook_testkit::TestBed;
use nook_types::{
    AuthSessionId, CreateUserNote, CreateUserNoteFolder, SealNoteRequest,
    SetVaultPassphraseRequest, UpdateUserNote, UpdateUserNoteFolder, UserNoteFolderId,
    UserNoteSummary,
};
use uuid::Uuid;

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

/// MAIN-210 AC-4: the schema has always allowed arbitrary nesting; the point is
/// that an MCP client can now REACH it. Five folders deep, built with nothing
/// but the tools a chat client has, with a note filed at the bottom — and the
/// folder listing carries the chain that renders the path.
#[tokio::test]
async fn folders_nest_to_arbitrary_depth_through_mcp() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping mcp notebook depth test — no DATABASE_URL");
        return;
    };
    let tenant = bed.tenant("mcpnbd").await;
    let (_u, alice) = bed.user(tenant, "owner").await;
    let backend = McpBackend {
        state: bed.app_state().await,
    };

    let names = ["Projects", "2026", "Q3", "Week 31", "Drafts"];
    let mut parent: Option<UserNoteFolderId> = None;
    let mut chain: Vec<UserNoteFolderId> = Vec::new();
    for name in names {
        let folder = backend
            .notebook_create_folder(
                alice,
                CreateUserNoteFolder {
                    name: name.into(),
                    parent_id: parent,
                },
            )
            .await
            .unwrap_or_else(|e| panic!("create {name}: {e}"));
        assert_eq!(folder.parent_id, parent);
        parent = Some(folder.id);
        chain.push(folder.id);
    }

    let deepest = parent.expect("five folders");
    let note = backend
        .notebook_create_note(
            alice,
            CreateUserNote {
                title: "Bottom".into(),
                content_md: "at depth five".into(),
                folder_id: Some(deepest),
            },
        )
        .await
        .expect("note at the bottom");
    assert_eq!(note.folder_id, Some(deepest));

    // list_folders reflects the paths: walking each folder's parent back to the
    // root reproduces the five names in order.
    let folders = backend.notebook_list_folders(alice).await.expect("folders");
    assert_eq!(folders.len(), names.len());
    let by_id: std::collections::HashMap<_, _> = folders.iter().map(|f| (f.id, f)).collect();
    let mut walked = Vec::new();
    let mut cursor = Some(deepest);
    while let Some(id) = cursor {
        let f = by_id.get(&id).expect("every ancestor is listed");
        walked.push(f.name.clone());
        cursor = f.parent_id;
    }
    walked.reverse();
    assert_eq!(walked, names, "the listed folders rebuild the whole chain");
    assert_eq!(chain.len(), names.len());

    // And the note's rendered path is the whole chain, so a client can show
    // where it landed without walking anything.
    let listed = backend
        .notebook_list_notes(alice, None)
        .await
        .expect("notes");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].path, names.join("/"));

    bed.teardown().await;
}

/// MAIN-210 AC-2: a note is filed anywhere and taken back out again — the root
/// included. Moving to the root is `Some(None)` on the tri-state, which is the
/// case the MCP surface could not express before.
#[tokio::test]
async fn a_note_moves_between_folders_and_back_to_the_root_through_mcp() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("mcpnbm").await;
    let (_u, alice) = bed.user(tenant, "owner").await;
    let backend = McpBackend {
        state: bed.app_state().await,
    };

    let inbox = backend
        .notebook_create_folder(
            alice,
            CreateUserNoteFolder {
                name: "Inbox".into(),
                parent_id: None,
            },
        )
        .await
        .expect("inbox");
    let archive = backend
        .notebook_create_folder(
            alice,
            CreateUserNoteFolder {
                name: "Archive".into(),
                parent_id: Some(inbox.id),
            },
        )
        .await
        .expect("archive");

    let note = backend
        .notebook_create_note(
            alice,
            CreateUserNote {
                title: "Wandering".into(),
                content_md: "body".into(),
                folder_id: None,
            },
        )
        .await
        .expect("note at the root");
    assert_eq!(note.folder_id, None);

    let move_to = |folder: Option<UserNoteFolderId>| UpdateUserNote {
        title: None,
        content_md: None,
        folder_id: Some(folder),
    };

    let filed = backend
        .notebook_update_note(alice, note.id, move_to(Some(inbox.id)))
        .await
        .expect("file it");
    assert_eq!(filed.folder_id, Some(inbox.id));

    let nested = backend
        .notebook_update_note(alice, note.id, move_to(Some(archive.id)))
        .await
        .expect("move it deeper");
    assert_eq!(nested.folder_id, Some(archive.id));

    // A title-only edit must not move it — the tri-state's `None` leg.
    let renamed = backend
        .notebook_update_note(
            alice,
            note.id,
            UpdateUserNote {
                title: Some("Still here".into()),
                content_md: None,
                folder_id: None,
            },
        )
        .await
        .expect("rename");
    assert_eq!(renamed.folder_id, Some(archive.id), "a rename moved a note");

    let rooted = backend
        .notebook_update_note(alice, note.id, move_to(None))
        .await
        .expect("back to the root");
    assert_eq!(rooted.folder_id, None, "a note cannot leave a folder");

    bed.teardown().await;
}

/// MAIN-210 AC-3: search over title and folder path, person-scoped. A sealed
/// note is found by its plaintext title — the seal covers the BODY, and no
/// summary has ever carried one — but its body stays unreadable over MCP, which
/// is the rule NG-1 is about.
#[tokio::test]
async fn notebook_search_matches_title_and_path_and_never_yields_a_sealed_body() {
    let Some(mut bed) = TestBed::new().await else {
        eprintln!("skipping mcp notebook search test — no DATABASE_URL");
        return;
    };
    let tenant = bed.tenant("mcpnbs").await;
    let (alice_user, alice) = bed.user(tenant, "owner").await;
    let (_ub, bob) = bed.user(tenant, "owner").await;
    let state = bed.app_state().await;
    let backend = McpBackend {
        state: state.clone(),
    };

    let work = backend
        .notebook_create_folder(
            alice,
            CreateUserNoteFolder {
                name: "Investments".into(),
                parent_id: None,
            },
        )
        .await
        .expect("folder");
    backend
        .notebook_create_note(
            alice,
            CreateUserNote {
                title: "Quarterly review".into(),
                content_md: "ciphertext-only body".into(),
                folder_id: Some(work.id),
            },
        )
        .await
        .expect("note in the folder");
    let loose = backend
        .notebook_create_note(
            alice,
            CreateUserNote {
                title: "Groceries".into(),
                content_md: "milk".into(),
                folder_id: None,
            },
        )
        .await
        .expect("root note");

    let titles =
        |hits: Vec<UserNoteSummary>| -> Vec<String> { hits.into_iter().map(|s| s.title).collect() };

    // By title fragment, case-insensitively.
    let by_title = backend
        .notebook_list_notes(alice, Some("quarter".into()))
        .await
        .expect("title search");
    assert_eq!(titles(by_title), vec!["Quarterly review".to_string()]);

    // By folder path — the reason search is more than a title filter.
    let by_path = backend
        .notebook_list_notes(alice, Some("invest".into()))
        .await
        .expect("path search");
    assert_eq!(titles(by_path), vec!["Quarterly review".to_string()]);

    // The body is encrypted at rest and is never searched.
    let by_body = backend
        .notebook_list_notes(alice, Some("ciphertext".into()))
        .await
        .expect("body search");
    assert!(by_body.is_empty(), "search reached into a note's body");

    // Search is person-scoped like everything else in the notebook.
    assert!(
        backend
            .notebook_list_notes(bob, Some("quarter".into()))
            .await
            .expect("bob searches")
            .is_empty(),
        "bob found alice's note"
    );

    // Seal the root note through the real route, then look for it over MCP.
    let auth = AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: alice_user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    };
    let pass = "correct horse staple";
    let vault = notebook::set_vault_passphrase(
        State(state.clone()),
        auth,
        Json(SetVaultPassphraseRequest {
            passphrase: pass.into(),
        }),
    )
    .await
    .expect("set the app password");
    assert!(vault.0.configured);
    let blob = crypto::seal_with_passphrase(b"milk, eggs", pass).expect("seal a blob");
    let sealed = notebook::seal_note(
        State(state.clone()),
        auth,
        Path(loose.id),
        Json(SealNoteRequest {
            salt: b64(&blob.salt),
            verifier: b64(&blob.verifier),
            ciphertext: b64(&blob.ciphertext),
            passphrase: pass.into(),
        }),
    )
    .await
    .expect("seal the note");
    assert!(sealed.0.sealed);

    let sealed_hits = backend
        .notebook_list_notes(alice, Some("grocer".into()))
        .await
        .expect("search a sealed note");
    assert_eq!(
        sealed_hits.len(),
        1,
        "a sealed note keeps its plaintext title"
    );
    assert!(sealed_hits[0].sealed, "and is flagged as sealed");

    // NG-1: reading it over MCP yields no body. The server cannot produce one.
    let read = backend
        .notebook_get_note(alice, loose.id)
        .await
        .expect("read the sealed note's metadata");
    assert!(read.sealed);
    assert!(
        read.content_md.is_none(),
        "a sealed body came back over MCP"
    );

    bed.teardown().await;
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
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
