//! MAIN-533: attachments on a ticket and on its comments.
//!
//! Handlers are driven directly, as the rest of this suite does. The upload
//! half is MAIN-532's and is exercised here only far enough to produce real
//! bytes in a real store — what is under test is the join, its permissions and
//! its two cascades, and a cascade that did not reach the bytes would pass
//! every row-level assertion.

use axum::body::Body;
use axum::extract::{FromRequest, Multipart, Path, State};
use axum::http::{header, Request, StatusCode};
use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::boards::delete_task;
use nook_control::routes::task_attachments::{
    attach_to_comment, attach_to_task, detach, get_one, list_for_comment, list_for_task, ListScope,
};
use nook_control::routes::task_detail::delete_comment;
use nook_control::routes::user_content::upload;
use nook_control::AppState;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

const BOUNDARY: &str = "nook-attach-boundary";

fn ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: true,
    }
}

/// A private disk root per test, so one test's objects are never another's.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        Scratch(std::env::temp_dir().join(format!("nook-attach-{tag}-{}", Uuid::now_v7().simple())))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn state_on(bed: &TestBed, scratch: &Scratch) -> AppState {
    let mut cfg = bed.config();
    cfg.dist_dir = scratch.0.to_string_lossy().into_owned();
    AppState::new(bed.db(), cfg, None).await
}

async fn multipart(filename: &str, content_type: &str, bytes: &[u8]) -> Multipart {
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/user-content")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .expect("a multipart request");
    Multipart::from_request(req, &()).await.expect("multipart")
}

/// Upload real bytes and return the record.
async fn put(state: &AppState, who: AuthCtx, filename: &str, bytes: &[u8]) -> UserContent {
    put_typed(state, who, filename, "application/octet-stream", bytes).await
}

/// The same, saying what the file claims to be — which is the whole input to
/// the inline-or-point decision (MAIN-534 AC-5).
async fn put_typed(
    state: &AppState,
    who: AuthCtx,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) -> UserContent {
    let res = upload(
        State(state.clone()),
        who,
        multipart(filename, content_type, bytes).await,
    )
    .await
    .expect("the upload succeeds");
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("a readable body");
    serde_json::from_slice(&body).expect("a UserContent record")
}

async fn board_in(bed: &TestBed, tenant: TenantId) -> BoardId {
    let id: BoardId = bed
        .db()
        .query_scalar(
            "INSERT INTO boards (id, tenant_id, name, key, provider)
             VALUES ($1, $2, 'b', $3, 'local') RETURNING id",
            params![
                BoardId::new(),
                tenant,
                format!("A{}", &Uuid::now_v7().simple().to_string()[..6]).to_uppercase()
            ],
        )
        .await
        .expect("board");
    bed.db()
        .exec(
            "INSERT INTO board_columns (id, board_id, name, position, type)
             VALUES ($1, $2, 'Todo', 0, 'unstarted')",
            params![Uuid::now_v7(), id],
        )
        .await
        .expect("column");
    id
}

async fn task_on(state: &AppState, tenant: TenantId, board: BoardId, creator: UserId) -> TaskItem {
    state
        .kanban
        .get("local")
        .expect("local provider")
        .create_task(
            tenant,
            board,
            Some(creator),
            CreateTaskRequest {
                title: "a ticket".into(),
                description: None,
                column_id: None,
                column_type: None,
                workspace_id: None,
                priority: None,
                type_: None,
                visibility: None,
                parent: None,
                labels: vec![],
            },
        )
        .await
        .expect("create task")
}

async fn comment_on(state: &AppState, tenant: TenantId, task: TaskId, author: UserId) -> Uuid {
    state
        .tasks
        .create_comment(nook_control::repo::tasks::NewComment {
            tenant,
            task,
            author_type: "user".into(),
            author_id: Some(author.0),
            author_name: "U".into(),
            body_md: "a comment".into(),
        })
        .await
        .expect("comment")
        .id
}

/// Whether the bytes are still in the store — the half no row-level assertion
/// can see.
async fn stored(state: &AppState, tenant: TenantId, content: Uuid) -> bool {
    let Some(row) = state
        .user_content
        .get(content, tenant)
        .await
        .expect("the row reads")
    else {
        return false;
    };
    state.artifacts.get(&row.storage_key).await.is_ok()
}

fn status_of(err: nook_control::error::ApiError) -> StatusCode {
    axum::response::IntoResponse::into_response(err).status()
}

/// AC-1 + AC-2: one record type, two parent kinds, each listing only its own —
/// a screenshot pasted into a comment is that comment's, not the ticket's.
#[tokio::test]
async fn both_parent_kinds_attach_and_list_separately() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let scratch = Scratch::new("both-kinds");
    let state = state_on(&bed, &scratch).await;
    let tenant = bed.tenant("attach").await;
    let (user, _) = bed.user(tenant, "member").await;
    let board = board_in(&bed, tenant).await;
    let task = task_on(&state, tenant, board, user).await;
    let comment = comment_on(&state, tenant, task.id, user).await;

    let on_task = put(&state, ctx(user, tenant), "spec.pdf", b"pdf bytes").await;
    let on_comment = put(&state, ctx(user, tenant), "shot.png", b"png bytes").await;

    let a = attach_to_task(
        State(state.clone()),
        ctx(user, tenant),
        Path(task.id.to_string()),
        axum::Json(AttachContentRequest {
            user_content_id: on_task.id,
        }),
    )
    .await
    .expect("attach to the ticket")
    .0;
    assert_eq!(a.parent_kind, "task");
    assert_eq!(a.filename, "spec.pdf");
    assert_eq!(a.size_bytes, 9);
    assert_eq!(a.attached_by, user);

    let _ = attach_to_comment(
        State(state.clone()),
        ctx(user, tenant),
        Path(comment),
        axum::Json(AttachContentRequest {
            user_content_id: on_comment.id,
        }),
    )
    .await
    .expect("attach to the comment");

    let ticket_list = list_for_task(
        State(state.clone()),
        ctx(user, tenant),
        Path(task.id.to_string()),
        axum::extract::Query(ListScope::default()),
    )
    .await
    .expect("list the ticket's")
    .0;
    assert_eq!(
        ticket_list.iter().map(|a| &a.filename).collect::<Vec<_>>(),
        vec!["spec.pdf"],
        "the comment's file is the comment's"
    );

    let comment_list = list_for_comment(State(state.clone()), ctx(user, tenant), Path(comment))
        .await
        .expect("list the comment's")
        .0;
    assert_eq!(
        comment_list.iter().map(|a| &a.filename).collect::<Vec<_>>(),
        vec!["shot.png"]
    );

    // `include=comments` is the ticket page's one request: the whole thread,
    // each record carrying the parent it belongs to.
    let thread = list_for_task(
        State(state.clone()),
        ctx(user, tenant),
        Path(task.id.to_string()),
        axum::extract::Query(ListScope {
            include: Some("comments".into()),
        }),
    )
    .await
    .expect("list the thread")
    .0;
    assert_eq!(thread.len(), 2);
    assert_eq!(
        thread
            .iter()
            .find(|a| a.filename == "shot.png")
            .map(|a| (a.parent_kind.as_str(), a.parent_id)),
        Some(("task_comment", comment))
    );

    // Attaching the same file twice is a double-click, not two attachments.
    let _ = attach_to_task(
        State(state.clone()),
        ctx(user, tenant),
        Path(task.id.to_string()),
        axum::Json(AttachContentRequest {
            user_content_id: on_task.id,
        }),
    )
    .await
    .expect("the second attach succeeds");
    assert_eq!(
        list_for_task(
            State(state.clone()),
            ctx(user, tenant),
            Path(task.id.to_string()),
            axum::extract::Query(ListScope::default()),
        )
        .await
        .expect("list again")
        .0
        .len(),
        1
    );

    bed.teardown().await;
}

/// AC-2's tenant scoping: another tenant's ticket, comment and attachment are
/// all 404 — never a 403, which would confirm they exist.
#[tokio::test]
async fn another_tenant_reaches_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let scratch = Scratch::new("tenant-isolation");
    let state = state_on(&bed, &scratch).await;
    let mine = bed.tenant("mine").await;
    let theirs = bed.tenant("theirs").await;
    let (me, _) = bed.user(mine, "member").await;
    // An OWNER over there: the refusal is the tenant boundary, not the role.
    let (them, _) = bed.user(theirs, "owner").await;
    let board = board_in(&bed, mine).await;
    let task = task_on(&state, mine, board, me).await;
    let comment = comment_on(&state, mine, task.id, me).await;

    let content = put(&state, ctx(me, mine), "secret.txt", b"mine").await;
    let attached = attach_to_task(
        State(state.clone()),
        ctx(me, mine),
        Path(task.id.to_string()),
        axum::Json(AttachContentRequest {
            user_content_id: content.id,
        }),
    )
    .await
    .expect("attach");

    for status in [
        status_of(
            list_for_task(
                State(state.clone()),
                ctx(them, theirs),
                Path(task.id.to_string()),
                axum::extract::Query(ListScope::default()),
            )
            .await
            .expect_err("no cross-tenant list"),
        ),
        status_of(
            list_for_comment(State(state.clone()), ctx(them, theirs), Path(comment))
                .await
                .expect_err("no cross-tenant comment list"),
        ),
        status_of(
            detach(State(state.clone()), ctx(them, theirs), Path(attached.id))
                .await
                .expect_err("no cross-tenant detach"),
        ),
    ] {
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    // Nor may a caller attach content they cannot read.
    let theirs_board = board_in(&bed, theirs).await;
    let theirs_task = task_on(&state, theirs, theirs_board, them).await;
    assert_eq!(
        status_of(
            attach_to_task(
                State(state.clone()),
                ctx(them, theirs),
                Path(theirs_task.id.to_string()),
                axum::Json(AttachContentRequest {
                    user_content_id: content.id,
                }),
            )
            .await
            .expect_err("another tenant's content is not attachable"),
        ),
        StatusCode::NOT_FOUND
    );

    assert!(
        stored(&state, mine, content.id).await,
        "and nothing was lost"
    );
    bed.teardown().await;
}

/// AC-6: the person who attached it may remove it; a third party may not; an
/// admin may. Removal takes the content row and the bytes.
#[tokio::test]
async fn removal_is_the_attachers_right_and_an_admins() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let scratch = Scratch::new("permission");
    let state = state_on(&bed, &scratch).await;
    let tenant = bed.tenant("perm").await;
    let (author, _) = bed.user(tenant, "member").await;
    let (stranger, _) = bed.user(tenant, "member").await;
    let (admin, _) = bed.user(tenant, "owner").await;
    let board = board_in(&bed, tenant).await;
    let task = task_on(&state, tenant, board, author).await;

    let attach = |who: UserId, name: &'static str| {
        let state = state.clone();
        let ident = task.id.to_string();
        async move {
            let content = put(&state, ctx(who, tenant), name, b"bytes").await;
            let row = attach_to_task(
                State(state.clone()),
                ctx(who, tenant),
                Path(ident),
                axum::Json(AttachContentRequest {
                    user_content_id: content.id,
                }),
            )
            .await
            .expect("attach");
            (content.id, row.id)
        }
    };

    let (first_content, first) = attach(author, "one.bin").await;

    assert_eq!(
        status_of(
            detach(State(state.clone()), ctx(stranger, tenant), Path(first))
                .await
                .expect_err("a third party may not remove it"),
        ),
        StatusCode::FORBIDDEN
    );
    assert!(stored(&state, tenant, first_content).await);

    detach(State(state.clone()), ctx(author, tenant), Path(first))
        .await
        .expect("the attacher may");
    assert!(
        !stored(&state, tenant, first_content).await,
        "the row and the bytes both went"
    );
    assert!(list_for_task(
        State(state.clone()),
        ctx(author, tenant),
        Path(task.id.to_string()),
        axum::extract::Query(ListScope::default()),
    )
    .await
    .expect("list")
    .0
    .is_empty());

    let (second_content, second) = attach(author, "two.bin").await;
    detach(State(state.clone()), ctx(admin, tenant), Path(second))
        .await
        .expect("an owner may remove someone else's");
    assert!(!stored(&state, tenant, second_content).await);

    bed.teardown().await;
}

/// AC-7: deleting a comment takes its attachments and their bytes; deleting the
/// ticket takes everything hanging off it, its comments' included.
#[tokio::test]
async fn deleting_a_parent_takes_its_attachments_and_bytes() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let scratch = Scratch::new("cascade");
    let state = state_on(&bed, &scratch).await;
    let tenant = bed.tenant("cascade").await;
    let (user, _) = bed.user(tenant, "member").await;
    let board = board_in(&bed, tenant).await;
    let task = task_on(&state, tenant, board, user).await;
    let doomed = comment_on(&state, tenant, task.id, user).await;

    let on_comment = put(&state, ctx(user, tenant), "shot.png", b"png").await;
    let _ = attach_to_comment(
        State(state.clone()),
        ctx(user, tenant),
        Path(doomed),
        axum::Json(AttachContentRequest {
            user_content_id: on_comment.id,
        }),
    )
    .await
    .expect("attach to the comment");

    delete_comment(State(state.clone()), ctx(user, tenant), Path(doomed))
        .await
        .expect("delete the comment");
    assert!(
        !stored(&state, tenant, on_comment.id).await,
        "the comment's image went with it"
    );

    // Now the ticket, with one file on it and one on a surviving comment.
    let survivor = comment_on(&state, tenant, task.id, user).await;
    let on_task = put(&state, ctx(user, tenant), "spec.pdf", b"pdf").await;
    let on_survivor = put(&state, ctx(user, tenant), "log.txt", b"log").await;
    let _ = attach_to_task(
        State(state.clone()),
        ctx(user, tenant),
        Path(task.id.to_string()),
        axum::Json(AttachContentRequest {
            user_content_id: on_task.id,
        }),
    )
    .await
    .expect("attach to the ticket");
    let _ = attach_to_comment(
        State(state.clone()),
        ctx(user, tenant),
        Path(survivor),
        axum::Json(AttachContentRequest {
            user_content_id: on_survivor.id,
        }),
    )
    .await
    .expect("attach to the surviving comment");

    delete_task(
        State(state.clone()),
        ctx(user, tenant),
        Path(task.id.to_string()),
    )
    .await
    .expect("delete the ticket");

    assert!(
        !stored(&state, tenant, on_task.id).await,
        "the ticket's file"
    );
    assert!(
        !stored(&state, tenant, on_survivor.id).await,
        "and its comment's — the join has no foreign key to follow"
    );

    bed.teardown().await;
}

/// AC-8: the board card's count, comments included, so the context is
/// discoverable before the ticket is opened.
#[tokio::test]
async fn the_card_count_sees_comments_too() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let scratch = Scratch::new("counts");
    let state = state_on(&bed, &scratch).await;
    let tenant = bed.tenant("counts").await;
    let (user, _) = bed.user(tenant, "member").await;
    let board = board_in(&bed, tenant).await;
    let with_files = task_on(&state, tenant, board, user).await;
    let bare = task_on(&state, tenant, board, user).await;
    let comment = comment_on(&state, tenant, with_files.id, user).await;

    for (parent_is_task, name) in [(true, "a.bin"), (true, "b.bin"), (false, "c.png")] {
        let content = put(&state, ctx(user, tenant), name, b"x").await;
        let req = axum::Json(AttachContentRequest {
            user_content_id: content.id,
        });
        if parent_is_task {
            let _ = attach_to_task(
                State(state.clone()),
                ctx(user, tenant),
                Path(with_files.id.to_string()),
                req,
            )
            .await
            .expect("attach");
        } else {
            let _ = attach_to_comment(State(state.clone()), ctx(user, tenant), Path(comment), req)
                .await
                .expect("attach");
        }
    }

    let mut cards = vec![with_files.clone(), bare.clone()];
    nook_control::services::attachments::fill_counts(&state, tenant, &mut cards)
        .await
        .expect("counts");
    assert_eq!(
        cards[0].attachment_count, 3,
        "two on the ticket, one on its comment"
    );
    assert_eq!(cards[1].attachment_count, 0);

    bed.teardown().await;
}

/// A node token — what a loop run's `nook` carries. Its tenant is the same;
/// its principal is not a person.
fn node_ctx(tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: UserId(Uuid::nil()),
        tenant_id: tenant,
        principal: Principal::Node(NodeId(Uuid::now_v7())),
        cookie_session: false,
    }
}

/// Attach `bytes` to `task`, and answer with the attachment id an agent would
/// be handed.
async fn attached(
    state: &AppState,
    who: AuthCtx,
    task: TaskId,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) -> Uuid {
    let content = put_typed(state, who, filename, content_type, bytes).await;
    attach_to_task(
        State(state.clone()),
        who,
        Path(task.to_string()),
        axum::Json(AttachContentRequest {
            user_content_id: content.id,
        }),
    )
    .await
    .expect("attach")
    .0
    .id
}

/// MAIN-534 AC-4: the id resolves for its own tenant, under a user token and
/// under a node token alike — and another tenant's id is a plain not-found,
/// never a leak of the filename it names.
#[tokio::test]
async fn one_attachment_resolves_by_id_and_only_inside_its_tenant() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let scratch = Scratch::new("get-one");
    let state = state_on(&bed, &scratch).await;
    let tenant = bed.tenant("mine").await;
    let (user, _) = bed.user(tenant, "member").await;
    let board = board_in(&bed, tenant).await;
    let task = task_on(&state, tenant, board, user).await;
    let id = attached(
        &state,
        ctx(user, tenant),
        task.id,
        "spec.md",
        "text/markdown",
        b"# a brief",
    )
    .await;

    let row = get_one(State(state.clone()), ctx(user, tenant), Path(id))
        .await
        .expect("the owner reads it")
        .0;
    assert_eq!(row.filename, "spec.md");
    assert_eq!(row.size_bytes, 9);

    // An agent's credential is not a person's, and reads the same record.
    let as_node = get_one(State(state.clone()), node_ctx(tenant), Path(id))
        .await
        .expect("a node token reads it too")
        .0;
    assert_eq!(as_node.id, row.id);

    let other = bed.tenant("theirs").await;
    let (stranger, _) = bed.user(other, "member").await;
    let err = get_one(State(state.clone()), ctx(stranger, other), Path(id))
        .await
        .expect_err("another tenant's id is not readable");
    assert_eq!(status_of(err), StatusCode::NOT_FOUND);

    // An id that never existed answers exactly the same, so neither is a probe.
    let err = get_one(
        State(state.clone()),
        ctx(user, tenant),
        Path(Uuid::now_v7()),
    )
    .await
    .expect_err("an unknown id");
    assert_eq!(status_of(err), StatusCode::NOT_FOUND);

    bed.teardown().await;
}

/// AC-2/AC-4: `nook attachments list <KEY>` is one request for the ticket AND
/// its comments, and a node token gets the same answer a person does.
#[tokio::test]
async fn the_thread_listing_serves_agents_too() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let scratch = Scratch::new("thread-list");
    let state = state_on(&bed, &scratch).await;
    let tenant = bed.tenant("thread").await;
    let (user, _) = bed.user(tenant, "member").await;
    let board = board_in(&bed, tenant).await;
    let task = task_on(&state, tenant, board, user).await;
    let comment = comment_on(&state, tenant, task.id, user).await;
    // The key the way a human sees it — `create_task` answers with the row, and
    // the key is a board join the enrichment adds, so it is read back here.
    let (board_key, number): (String, i32) = bed
        .db()
        .query_one(
            "SELECT b.key, t.number FROM tasks t
               JOIN boards b ON b.id = t.board_id WHERE t.id = $1",
            params![task.id],
        )
        .await
        .expect("the card's key");
    let key = format!("{board_key}-{number}");

    attached(
        &state,
        ctx(user, tenant),
        task.id,
        "spec.md",
        "text/markdown",
        b"brief",
    )
    .await;
    let on_comment = put(&state, ctx(user, tenant), "logs.zip", b"PK\x03\x04").await;
    let _ = attach_to_comment(
        State(state.clone()),
        ctx(user, tenant),
        Path(comment),
        axum::Json(AttachContentRequest {
            user_content_id: on_comment.id,
        }),
    )
    .await
    .expect("attach to the comment");

    for (who, label) in [
        (ctx(user, tenant), "a person"),
        (node_ctx(tenant), "a node"),
    ] {
        let rows = nook_control::services::attachments::list_thread_readable(
            &state,
            tenant,
            who.user_id,
            &key,
        )
        .await
        .unwrap_or_else(|_| panic!("{label} lists the thread"));
        assert_eq!(rows.len(), 2, "{label}: the ticket's and the comment's");
        assert!(rows.iter().any(|r| r.parent_kind == "task_comment"));
    }

    // The key resolves as well as the uuid — a key is what an agent is handed.
    let other = bed.tenant("elsewhere").await;
    let (stranger, _) = bed.user(other, "member").await;
    let err =
        nook_control::services::attachments::list_thread_readable(&state, other, stranger, &key)
            .await
            .expect_err("another tenant cannot list this card");
    assert_eq!(status_of(err), StatusCode::NOT_FOUND);

    bed.teardown().await;
}

/// AC-5: text comes back as text; a binary file comes back as a pointer to the
/// CLI, never as bytes in the transcript.
#[tokio::test]
async fn an_agent_reads_text_and_is_pointed_at_everything_else() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let scratch = Scratch::new("read-content");
    let state = state_on(&bed, &scratch).await;
    let tenant = bed.tenant("read").await;
    let (user, _) = bed.user(tenant, "member").await;
    let board = board_in(&bed, tenant).await;
    let task = task_on(&state, tenant, board, user).await;

    let doc = attached(
        &state,
        ctx(user, tenant),
        task.id,
        "spec.md",
        "text/markdown",
        b"# AC-1\nread me",
    )
    .await;
    let png = attached(
        &state,
        ctx(user, tenant),
        task.id,
        "shot.png",
        "image/png",
        b"\x89PNG not really",
    )
    .await;

    let read = |id| {
        let state = state.clone();
        async move {
            nook_control::services::attachments::read_content(&state, tenant, user, id)
                .await
                .expect("readable")
        }
    };

    let text = read(doc).await;
    assert_eq!(text.content.as_deref(), Some("# AC-1\nread me"));
    assert!(text.not_inlined.is_none());

    let binary = read(png).await;
    assert!(binary.content.is_none(), "no bytes reach the transcript");
    let hint = binary.not_inlined.expect("a pointer instead");
    assert!(
        hint.contains(&format!("nook attachments get {png}")),
        "the pointer names the command and the id: {hint}"
    );
    assert_eq!(binary.filename, "shot.png", "listed, just not inlined");

    // A file whose type says text but whose bytes are not is the uploader's
    // mistake — it points rather than returning replacement characters.
    let lying = attached(
        &state,
        ctx(user, tenant),
        task.id,
        "notes.txt",
        "text/plain",
        &[0xff, 0xfe, 0x00],
    )
    .await;
    assert!(read(lying).await.content.is_none());

    // And another tenant's id is the same 404 the record lookup gives.
    let other = bed.tenant("read-other").await;
    let (stranger, _) = bed.user(other, "member").await;
    let err = nook_control::services::attachments::read_content(&state, other, stranger, doc)
        .await
        .expect_err("cross-tenant");
    assert_eq!(status_of(err), StatusCode::NOT_FOUND);

    bed.teardown().await;
}

/// AC-5's other half: text that is too big for a reply is pointed at as well.
/// "Is it text" and "does it belong in a context window" are different
/// questions, and a 25 MiB log answers yes to the first.
#[tokio::test]
async fn text_too_large_to_inline_is_pointed_at() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let scratch = Scratch::new("big-text");
    let state = state_on(&bed, &scratch).await;
    let tenant = bed.tenant("big").await;
    let (user, _) = bed.user(tenant, "member").await;
    let board = board_in(&bed, tenant).await;
    let task = task_on(&state, tenant, board, user).await;

    let big = vec![b'x'; 300 * 1024];
    let id = attached(
        &state,
        ctx(user, tenant),
        task.id,
        "run.log",
        "text/plain",
        &big,
    )
    .await;

    let read = nook_control::services::attachments::read_content(&state, tenant, user, id)
        .await
        .expect("readable");
    assert!(read.content.is_none());
    let hint = read.not_inlined.expect("a pointer");
    assert!(hint.contains("inline limit"), "{hint}");
    assert_eq!(read.size_bytes, big.len() as i64);

    bed.teardown().await;
}
