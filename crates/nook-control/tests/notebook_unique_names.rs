//! A notebook name is unique where it sits (MAIN-574): a folder name per
//! (person, parent), a note title per (person, folder) — including at the ROOT,
//! where both keys are NULL.
//!
//! The root cases are the ones that fail if the NULL-equating half is forgotten
//! (`NULLS NOT DISTINCT` on Postgres, a `COALESCE` expression index on SQLite,
//! MAIN-388's lesson), so they are asserted head-on rather than left to the
//! nested cases. Everything here runs on **whichever engine the bed is on**, so
//! the SQLite leg re-asks every question of the twin migration.

use nook_control::error::ApiError;
use nook_control::routes::notebook;
use nook_db::{params, Db, Engine};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

use axum::extract::{Path, Query, State};
use axum::Json;

use nook_control::auth::{AuthCtx, Principal};

fn auth(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: false,
    }
}

/// A bed plus one signed-in person — every test here needs exactly this.
async fn bed_and_person() -> Option<(TestBed, AuthCtx, Uuid)> {
    let bed = TestBed::new().await?;
    let tenant = bed.tenant("n574").await;
    let (user, person) = bed.user(tenant, "owner").await;
    Some((bed, auth(user, tenant), person))
}

fn folder(name: &str, parent: Option<UserNoteFolderId>) -> CreateUserNoteFolder {
    CreateUserNoteFolder {
        name: name.into(),
        parent_id: parent,
    }
}

fn note(title: &str, folder: Option<UserNoteFolderId>) -> CreateUserNote {
    CreateUserNote {
        title: title.into(),
        content_md: "body".into(),
        folder_id: folder,
    }
}

/// The conflict, with the taken name in the message — a 409 a client can render
/// as words, which is the whole difference from a raw constraint violation.
#[track_caller]
fn assert_conflict_naming<T: std::fmt::Debug>(r: Result<T, ApiError>, name: &str) {
    match r {
        Err(ApiError::Conflict(msg)) => assert!(
            msg.contains(name),
            "the 409 must name the taken name; got {msg:?}"
        ),
        other => panic!("expected a 409 naming {name:?}, got {other:?}"),
    }
}

// ── folders ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_folder_name_is_unique_per_parent_and_free_under_another() {
    let Some((mut bed, me, _person)) = bed_and_person().await else {
        return;
    };
    let state = bed.app_state().await;

    let work = notebook::create_folder(State(state.clone()), me, Json(folder("Work", None)))
        .await
        .expect("a root folder")
        .0;
    let home = notebook::create_folder(State(state.clone()), me, Json(folder("Home", None)))
        .await
        .expect("a second root folder")
        .0;

    let _ = notebook::create_folder(
        State(state.clone()),
        me,
        Json(folder("Ideas", Some(work.id))),
    )
    .await
    .expect("Ideas under Work");
    assert_conflict_naming(
        notebook::create_folder(
            State(state.clone()),
            me,
            Json(folder("Ideas", Some(work.id))),
        )
        .await
        .map(|j| j.0),
        "Ideas",
    );
    // The two live `Ideas` folders are the real case this must keep working.
    let _ = notebook::create_folder(
        State(state.clone()),
        me,
        Json(folder("Ideas", Some(home.id))),
    )
    .await
    .expect("Ideas under Home is a different place");

    bed.teardown().await;
}

/// The NULL case (AC-2). `parent_id IS NULL` is one place — the root — and the
/// SQL default would make every row in it distinct from every other.
#[tokio::test]
async fn two_root_folders_cannot_share_a_name() {
    let Some((mut bed, me, _person)) = bed_and_person().await else {
        return;
    };
    let state = bed.app_state().await;

    let _ = notebook::create_folder(State(state.clone()), me, Json(folder("Ideas", None)))
        .await
        .expect("a root folder");
    assert_conflict_naming(
        notebook::create_folder(State(state.clone()), me, Json(folder("Ideas", None)))
            .await
            .map(|j| j.0),
        "Ideas",
    );

    bed.teardown().await;
}

#[tokio::test]
async fn renaming_or_moving_a_folder_onto_a_taken_name_is_a_conflict() {
    let Some((mut bed, me, _person)) = bed_and_person().await else {
        return;
    };
    let state = bed.app_state().await;

    let work = notebook::create_folder(State(state.clone()), me, Json(folder("Work", None)))
        .await
        .expect("Work")
        .0;
    let _ = notebook::create_folder(
        State(state.clone()),
        me,
        Json(folder("Ideas", Some(work.id))),
    )
    .await
    .expect("Ideas under Work");
    let notes = notebook::create_folder(
        State(state.clone()),
        me,
        Json(folder("Notes", Some(work.id))),
    )
    .await
    .expect("Notes under Work")
    .0;
    // Same name, a different parent — legal until it moves.
    let stray = notebook::create_folder(State(state.clone()), me, Json(folder("Ideas", None)))
        .await
        .expect("Ideas at the root")
        .0;

    assert_conflict_naming(
        notebook::update_folder(
            State(state.clone()),
            me,
            Path(notes.id),
            Json(UpdateUserNoteFolder {
                name: Some("Ideas".into()),
                parent_id: None,
            }),
        )
        .await
        .map(|j| j.0),
        "Ideas",
    );

    // A MOVE with no rename collides just the same: the key is the pair.
    assert_conflict_naming(
        notebook::update_folder(
            State(state.clone()),
            me,
            Path(stray.id),
            Json(UpdateUserNoteFolder {
                name: None,
                parent_id: Some(Some(work.id)),
            }),
        )
        .await
        .map(|j| j.0),
        "Ideas",
    );

    // Renaming a folder to the name it already has is not a collision with
    // itself — the check excludes the row being edited.
    let _ = notebook::update_folder(
        State(state.clone()),
        me,
        Path(notes.id),
        Json(UpdateUserNoteFolder {
            name: Some("Notes".into()),
            parent_id: None,
        }),
    )
    .await
    .expect("a no-op rename is not a self-collision");

    bed.teardown().await;
}

// ── notes ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_note_title_is_unique_per_folder_including_the_root() {
    let Some((mut bed, me, _person)) = bed_and_person().await else {
        return;
    };
    let state = bed.app_state().await;

    let work = notebook::create_folder(State(state.clone()), me, Json(folder("Work", None)))
        .await
        .expect("Work")
        .0;
    let home = notebook::create_folder(State(state.clone()), me, Json(folder("Home", None)))
        .await
        .expect("Home")
        .0;

    let _ = notebook::create_note(
        State(state.clone()),
        me,
        Json(note("Roadmap", Some(work.id))),
    )
    .await
    .expect("Roadmap in Work");
    assert_conflict_naming(
        notebook::create_note(
            State(state.clone()),
            me,
            Json(note("Roadmap", Some(work.id))),
        )
        .await
        .map(|j| j.0),
        "Roadmap",
    );
    let _ = notebook::create_note(
        State(state.clone()),
        me,
        Json(note("Roadmap", Some(home.id))),
    )
    .await
    .expect("the same title in another folder is a different note");

    // And at the root, where folder_id IS NULL.
    let _ = notebook::create_note(State(state.clone()), me, Json(note("Roadmap", None)))
        .await
        .expect("Roadmap at the root");
    assert_conflict_naming(
        notebook::create_note(State(state.clone()), me, Json(note("Roadmap", None)))
            .await
            .map(|j| j.0),
        "Roadmap",
    );

    bed.teardown().await;
}

#[tokio::test]
async fn renaming_or_moving_a_note_onto_a_taken_title_is_a_conflict() {
    let Some((mut bed, me, _person)) = bed_and_person().await else {
        return;
    };
    let state = bed.app_state().await;

    let work = notebook::create_folder(State(state.clone()), me, Json(folder("Work", None)))
        .await
        .expect("Work")
        .0;
    let _ = notebook::create_note(
        State(state.clone()),
        me,
        Json(note("Roadmap", Some(work.id))),
    )
    .await
    .expect("Roadmap in Work");
    let other = notebook::create_note(
        State(state.clone()),
        me,
        Json(note("Scratch", Some(work.id))),
    )
    .await
    .expect("Scratch in Work")
    .0;
    let at_root = notebook::create_note(State(state.clone()), me, Json(note("Roadmap", None)))
        .await
        .expect("Roadmap at the root")
        .0;

    assert_conflict_naming(
        notebook::update_note(
            State(state.clone()),
            me,
            Path(other.id),
            Json(UpdateUserNote {
                title: Some("Roadmap".into()),
                content_md: None,
                folder_id: None,
            }),
        )
        .await
        .map(|j| j.0),
        "Roadmap",
    );

    assert_conflict_naming(
        notebook::update_note(
            State(state.clone()),
            me,
            Path(at_root.id),
            Json(UpdateUserNote {
                title: None,
                content_md: None,
                folder_id: Some(Some(work.id)),
            }),
        )
        .await
        .map(|j| j.0),
        "Roadmap",
    );

    // A body-only edit touches neither half of the key.
    let _ = notebook::update_note(
        State(state.clone()),
        me,
        Path(other.id),
        Json(UpdateUserNote {
            title: None,
            content_md: Some("rewritten".into()),
            folder_id: None,
        }),
    )
    .await
    .expect("a body edit is not a name collision");

    bed.teardown().await;
}

// ── deleting a folder is a move too ──────────────────────────────────────────

/// Deleting a folder lifts its contents to the folder's own parent, and that is
/// a MOVE — subject to the same uniqueness as a `PATCH`. Two folders named
/// `Archive` under different parents are legal, so the delete has to refuse
/// with a 409 rather than let the index abort the transaction and surface as
/// `500 internal error`, leaving a folder that will not go away.
#[tokio::test]
async fn deleting_a_folder_whose_child_would_collide_is_a_conflict() {
    let Some((mut bed, me, _person)) = bed_and_person().await else {
        return;
    };
    let state = bed.app_state().await;

    let work = notebook::create_folder(State(state.clone()), me, Json(folder("Work", None)))
        .await
        .expect("Work")
        .0;
    let _ = notebook::create_folder(State(state.clone()), me, Json(folder("Archive", None)))
        .await
        .expect("Archive at the root");
    let _ = notebook::create_folder(
        State(state.clone()),
        me,
        Json(folder("Archive", Some(work.id))),
    )
    .await
    .expect("Archive under Work — legal, a different parent");

    assert_conflict_naming(
        notebook::delete_folder(State(state.clone()), me, Path(work.id))
            .await
            .map(|s| s.as_u16()),
        "Archive",
    );
    let left = notebook::list_folders(State(state.clone()), me)
        .await
        .expect("list")
        .0;
    assert_eq!(
        left.len(),
        3,
        "the refusal is whole: nothing moved and the folder is still there"
    );

    bed.teardown().await;
}

/// The note half of the same rule, and the same 409.
#[tokio::test]
async fn deleting_a_folder_whose_note_would_collide_is_a_conflict() {
    let Some((mut bed, me, _person)) = bed_and_person().await else {
        return;
    };
    let state = bed.app_state().await;

    let work = notebook::create_folder(State(state.clone()), me, Json(folder("Work", None)))
        .await
        .expect("Work")
        .0;
    let _ = notebook::create_note(State(state.clone()), me, Json(note("Roadmap", None)))
        .await
        .expect("Roadmap at the root");
    let _ = notebook::create_note(
        State(state.clone()),
        me,
        Json(note("Roadmap", Some(work.id))),
    )
    .await
    .expect("Roadmap in Work");

    assert_conflict_naming(
        notebook::delete_folder(State(state.clone()), me, Path(work.id))
            .await
            .map(|s| s.as_u16()),
        "Roadmap",
    );
    let notes = notebook::list_notes(State(state.clone()), me, Query(Default::default()))
        .await
        .expect("list notes")
        .0;
    assert_eq!(notes.len(), 2, "neither note moved");

    bed.teardown().await;
}

/// A child carrying the deleted folder's OWN name is not a collision — the row
/// it would clash with is the one going away — so `Work/Work` stays deletable.
/// It is the case that fails if the delete moves contents before removing the
/// folder, and the reason the same-named child travels last.
#[tokio::test]
async fn a_child_named_after_the_folder_being_deleted_rises_into_its_place() {
    let Some((mut bed, me, _person)) = bed_and_person().await else {
        return;
    };
    let state = bed.app_state().await;

    let outer = notebook::create_folder(State(state.clone()), me, Json(folder("Work", None)))
        .await
        .expect("Work at the root")
        .0;
    let inner = notebook::create_folder(
        State(state.clone()),
        me,
        Json(folder("Work", Some(outer.id))),
    )
    .await
    .expect("Work inside Work")
    .0;
    let deep = notebook::create_folder(
        State(state.clone()),
        me,
        Json(folder("Deep", Some(outer.id))),
    )
    .await
    .expect("a second child, which moves the ordinary way")
    .0;

    notebook::delete_folder(State(state.clone()), me, Path(outer.id))
        .await
        .expect("the outer Work deletes");

    let left = notebook::list_folders(State(state.clone()), me)
        .await
        .expect("list")
        .0;
    let mut by_id: Vec<(UserNoteFolderId, Option<UserNoteFolderId>)> =
        left.iter().map(|f| (f.id, f.parent_id)).collect();
    by_id.sort_by_key(|(id, _)| *id);
    let mut want = vec![(inner.id, None), (deep.id, None)];
    want.sort_by_key(|(id, _)| *id);
    assert_eq!(by_id, want, "both children rose to the root");

    bed.teardown().await;
}

/// A folder nested under a parent, holding a child of its own name, while the
/// ROOT holds that name too. The same-named child reaches its destination by
/// way of the root, so this is refused up front rather than failing mid-move.
#[tokio::test]
async fn a_same_named_child_that_cannot_pass_through_the_root_is_a_conflict() {
    let Some((mut bed, me, _person)) = bed_and_person().await else {
        return;
    };
    let state = bed.app_state().await;

    let home = notebook::create_folder(State(state.clone()), me, Json(folder("Home", None)))
        .await
        .expect("Home")
        .0;
    let _ = notebook::create_folder(State(state.clone()), me, Json(folder("Work", None)))
        .await
        .expect("Work at the root");
    let work = notebook::create_folder(
        State(state.clone()),
        me,
        Json(folder("Work", Some(home.id))),
    )
    .await
    .expect("Home/Work")
    .0;
    let _ = notebook::create_folder(
        State(state.clone()),
        me,
        Json(folder("Work", Some(work.id))),
    )
    .await
    .expect("Home/Work/Work");

    assert_conflict_naming(
        notebook::delete_folder(State(state.clone()), me, Path(work.id))
            .await
            .map(|s| s.as_u16()),
        "Work",
    );
    let left = notebook::list_folders(State(state.clone()), me)
        .await
        .expect("list")
        .0;
    assert_eq!(left.len(), 4, "nothing moved and nothing was deleted");

    bed.teardown().await;
}

// ── the separator (AC-5) ─────────────────────────────────────────────────────

#[tokio::test]
async fn a_name_containing_a_slash_is_refused() {
    let Some((mut bed, me, _person)) = bed_and_person().await else {
        return;
    };
    let state = bed.app_state().await;

    let bad_folder = notebook::create_folder(State(state.clone()), me, Json(folder("a/b", None)))
        .await
        .map(|j| j.0);
    assert!(
        matches!(&bad_folder, Err(ApiError::BadRequest(m)) if m.contains('/')),
        "a folder name carrying the path separator is a 400 naming it; got {bad_folder:?}"
    );

    let bad_note = notebook::create_note(State(state.clone()), me, Json(note("a/b", None)))
        .await
        .map(|j| j.0);
    assert!(
        matches!(&bad_note, Err(ApiError::BadRequest(m)) if m.contains('/')),
        "and so is a note title; got {bad_note:?}"
    );

    bed.teardown().await;
}

// ── the migration's own de-dup (AC-3) ────────────────────────────────────────

/// This card's migration, for the engine the bed is on. Read from the file the
/// migrator embeds, so the test cannot drift from what a deploy runs.
fn migration_sql(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => include_str!("../migrations/0071_notebook_unique_names.sql"),
        Engine::Sqlite => include_str!("../migrations_sqlite/0071_notebook_unique_names.sql"),
    }
}

/// Run it statement by statement — one prepared statement per command is all
/// either driver accepts. Comment lines go first because a prose `;` would
/// otherwise split one in half; no statement carries a `;` of its own.
async fn run_migration(bed: &TestBed) {
    let sql: String = migration_sql(bed.engine())
        .lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    for statement in sql.split(';') {
        if statement.trim().is_empty() {
            continue;
        }
        bed.db()
            .exec(statement, params![])
            .await
            .unwrap_or_else(|e| panic!("migration statement failed: {e}\n{statement}"));
    }
}

/// Put the bed back in the state a notebook that drifted before the deploy was
/// in: the tables, without this card's indexes.
async fn drop_the_indexes(bed: &TestBed) {
    for index in [
        "user_note_folders_person_parent_name_uniq",
        "user_notes_person_folder_title_uniq",
    ] {
        bed.db()
            .exec(&format!("DROP INDEX IF EXISTS {index}"), params![])
            .await
            .expect("drop the index this migration adds");
    }
}

/// A folder inserted BENEATH the routes, which is the only way to write a
/// collision once the index exists.
async fn seed_folder(
    bed: &TestBed,
    person: Uuid,
    parent: Option<UserNoteFolderId>,
    name: &str,
    at: chrono::DateTime<chrono::Utc>,
) -> UserNoteFolderId {
    let id = UserNoteFolderId::new();
    bed.db()
        .exec(
            "INSERT INTO user_note_folders (id, person_id, parent_id, name, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $5)",
            params![id, person, parent.map(|p| p.0), name, at],
        )
        .await
        .expect("seed a folder");
    id
}

async fn name_of(bed: &TestBed, id: UserNoteFolderId) -> String {
    bed.db()
        .query_scalar(
            "SELECT name FROM user_note_folders WHERE id = $1",
            params![id],
        )
        .await
        .expect("read the name back")
}

/// One pass over a thoroughly tangled notebook, because the suffix rule is only
/// worth what an adversarial case says it is. Every name here is one a previous
/// de-dup could have written, including a suffix too long to be one — which is
/// excluded from the count rather than overflowing the cast.
#[tokio::test]
async fn the_migration_is_a_fixed_point_on_a_tangled_notebook() {
    let Some((mut bed, _me, person)) = bed_and_person().await else {
        return;
    };
    drop_the_indexes(&bed).await;

    let tangle = [
        "Ideas",
        "Ideas",
        "Ideas",
        "Ideas (2)",
        "Ideas (2)",
        "Ideas (3)",
        "Ideas (2) (2)",
        "Ideas (2) (2)",
        "Notes",
        "Ideas (99999999999999999999)",
    ];
    let base = chrono::Utc::now() - chrono::Duration::seconds(60);
    for (i, name) in tangle.iter().enumerate() {
        seed_folder(
            &bed,
            person,
            None,
            name,
            base + chrono::Duration::seconds(i as i64),
        )
        .await;
    }

    run_migration(&bed).await;

    let names: Vec<String> = bed
        .db()
        .query_scalar_all(
            "SELECT name FROM user_note_folders WHERE person_id = $1 ORDER BY name",
            params![person],
        )
        .await
        .expect("read the names back");
    assert_eq!(names.len(), tangle.len(), "every row survives the de-dup");
    let distinct: std::collections::HashSet<&String> = names.iter().collect();
    assert_eq!(
        distinct.len(),
        names.len(),
        "one pass leaves no duplicate at all: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "Ideas (99999999999999999999)"),
        "a suffix too long to be a number is left alone, not renamed: {names:?}"
    );

    bed.teardown().await;
}

/// A database that already holds duplicates is RENAMED into shape, never
/// truncated and never failed (AC-3). Asserted by removing the indexes this
/// card's migration created, re-creating the collision underneath, and running
/// the migration again — which is exactly the state a notebook that drifted
/// before the deploy would be in.
#[tokio::test]
async fn the_migration_renames_a_collision_rather_than_failing() {
    let Some((mut bed, _me, person)) = bed_and_person().await else {
        return;
    };
    drop_the_indexes(&bed).await;

    // Three at the ROOT, so the de-dup's NULL-equating is exercised too.
    let base = chrono::Utc::now() - chrono::Duration::seconds(30);
    let first = seed_folder(&bed, person, None, "Ideas", base).await;
    let second = seed_folder(
        &bed,
        person,
        None,
        "Ideas",
        base + chrono::Duration::seconds(1),
    )
    .await;
    let third = seed_folder(
        &bed,
        person,
        None,
        "Ideas",
        base + chrono::Duration::seconds(2),
    )
    .await;
    // TWO rows already carrying the name a naive rename would produce. This is
    // the shape no number of ` (2)`-counting passes converges on: one pass
    // would send the second `Ideas` onto `Ideas (2)` while the second
    // `Ideas (2)` became `Ideas (2) (2)`, and the pass after it would
    // manufacture the collision after that. The suffix counts on from the
    // highest one already in use instead, which makes a single pass a fixed
    // point.
    let squatter = seed_folder(
        &bed,
        person,
        None,
        "Ideas (2)",
        base + chrono::Duration::seconds(3),
    )
    .await;
    let squatter_twin = seed_folder(
        &bed,
        person,
        None,
        "Ideas (2)",
        base + chrono::Duration::seconds(4),
    )
    .await;

    run_migration(&bed).await;

    assert_eq!(
        name_of(&bed, first).await,
        "Ideas",
        "the earliest row keeps the name it had"
    );
    assert_eq!(
        name_of(&bed, second).await,
        "Ideas (3)",
        "counting on from the `Ideas (2)` that already existed, not from 2"
    );
    assert_eq!(name_of(&bed, third).await, "Ideas (4)");
    assert_eq!(
        name_of(&bed, squatter).await,
        "Ideas (2)",
        "a row that collided with nothing keeps its name"
    );
    assert_eq!(
        name_of(&bed, squatter_twin).await,
        "Ideas (2) (2)",
        "its own duplicate is renamed in the same pass"
    );

    let survivors: i64 = bed
        .db()
        .query_scalar(
            "SELECT count(*) FROM user_note_folders WHERE person_id = $1",
            params![person],
        )
        .await
        .expect("count");
    assert_eq!(survivors, 5, "nothing is deleted and nothing is merged");

    // And the index is back, so the state that produced this cannot recur.
    let again = bed
        .db()
        .exec(
            "INSERT INTO user_note_folders (id, person_id, parent_id, name)
             VALUES ($1, $2, NULL, 'Ideas')",
            params![UserNoteFolderId::new(), person],
        )
        .await;
    assert!(
        matches!(&again, Err(e) if e.is_unique_violation()),
        "the unique index must exist after the migration; got {again:?}"
    );

    bed.teardown().await;
}
