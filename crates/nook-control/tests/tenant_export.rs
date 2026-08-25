//! MAIN-659: the tenant export archive — what it carries, what it refuses to
//! carry, and who may ask for it.
//!
//! The handler is driven directly, as the rest of this suite does. The archive
//! it produces is read back with the `tar` crate rather than by re-deriving the
//! layout here: a reader that shares the writer's assumptions proves nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::tenant_export::export;
use nook_control::services::tenant_archive::{self as fmt, Column};
use nook_control::AppState;
use nook_db::{params, Db, DbPool};
use nook_testkit::TestBed;
use nook_types::*;
use serde_json::Value;
use uuid::Uuid;

// ── harness ─────────────────────────────────────────────────────────────────

/// A private disk root per test, so one test's blobs are never another's.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        Scratch(std::env::temp_dir().join(format!("nook-export-{tag}-{}", Uuid::now_v7().simple())))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn state_on(bed: &TestBed, scratch: &Scratch) -> AppState {
    let mut cfg = bed.config();
    cfg.user_content_dir = scratch.0.to_string_lossy().into_owned();
    AppState::new(bed.db(), cfg, None).await
}

fn ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: true,
    }
}

fn node_ctx(node: NodeId, user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        principal: Principal::Node(node),
        ..ctx(user, tenant)
    }
}

/// A user in `tenant` holding `role`, with the membership grant the export gate
/// actually reads (`tenant_members`, not `users.role`).
async fn member(bed: &TestBed, tenant: TenantId, role: &str) -> UserId {
    let (user, _person) = bed.user(tenant, role).await;
    bed.db()
        .exec(
            "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
             VALUES ($1, $2, 'user', $3, $4)",
            params![Uuid::new_v4(), tenant, user.0, role.to_string()],
        )
        .await
        .expect("grant membership");
    user
}

/// Every member of an archive, by path.
struct Archive(BTreeMap<String, Vec<u8>>);

impl Archive {
    fn paths(&self) -> BTreeSet<&str> {
        self.0.keys().map(String::as_str).collect()
    }

    fn text(&self, path: &str) -> &str {
        std::str::from_utf8(
            self.0
                .get(path)
                .unwrap_or_else(|| panic!("no {path} in the archive")),
        )
        .expect("utf-8")
    }

    fn manifest(&self) -> Value {
        serde_json::from_slice(&self.0["manifest.json"]).expect("a readable manifest")
    }

    /// One table's rows, parsed.
    fn rows(&self, table: &str) -> Vec<Value> {
        self.text(&fmt::table_path(table))
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("each line is one JSON object"))
            .collect()
    }

    /// The whole archive as one byte string, for "this must appear nowhere"
    /// assertions.
    fn every_byte(&self) -> Vec<u8> {
        let mut all = Vec::new();
        for (path, body) in &self.0 {
            all.extend_from_slice(path.as_bytes());
            all.extend_from_slice(body);
        }
        all
    }
}

/// Run the export and read the archive back.
async fn export_ok(state: &AppState, who: AuthCtx, tenant: TenantId) -> Archive {
    let res = export(State(state.clone()), who, Path(tenant))
        .await
        .expect("the export is allowed");
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/gzip")
    );
    let disposition = res
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        disposition.contains(".tar.gz"),
        "the download names a .tar.gz: {disposition}"
    );

    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("the archive streams to completion")
        .to_vec();

    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..]));
    let mut members = BTreeMap::new();
    let mut first = None;
    for entry in archive.entries().expect("a readable tarball") {
        let mut entry = entry.expect("a readable member");
        let path = entry.path().expect("a member path").display().to_string();
        first.get_or_insert(path.clone());
        let mut body = Vec::new();
        entry.read_to_end(&mut body).expect("member bytes");
        members.insert(path, body);
    }
    assert_eq!(
        first.as_deref(),
        Some("manifest.json"),
        "the manifest is the first member, so a reader learns the format before anything else"
    );
    Archive(members)
}

/// The 403 an unauthorized caller gets, as a status.
async fn export_status(state: &AppState, who: AuthCtx, tenant: TenantId) -> StatusCode {
    use axum::response::IntoResponse;
    match export(State(state.clone()), who, Path(tenant)).await {
        Ok(res) => res.status(),
        Err(e) => e.into_response().status(),
    }
}

async fn count(db: &DbPool, sql: &str, tenant: TenantId) -> i64 {
    #[derive(nook_db::FromDbRow)]
    struct N {
        n: i64,
    }
    let row: N = db.query_one(sql, params![tenant]).await.expect("count");
    row.n
}

// ── the seed ────────────────────────────────────────────────────────────────

/// One row in every table the archive carries, plus the shapes the format has
/// to get right: two boards, a task with a comment, a relation, a description
/// revision, a report, a label, and an attachment whose bytes are in the store.
struct Seeded {
    owner: UserId,
    sha: String,
}

async fn seed(bed: &TestBed, state: &AppState, tenant: TenantId, tag: &str) -> Seeded {
    let db = bed.db();
    let owner = member(bed, tenant, "owner").await;

    let ws = bed.workspace(tenant).await;
    let (board, other_board) = (Uuid::new_v4(), Uuid::new_v4());
    for (id, name, key) in [
        (board, format!("{tag} board"), format!("{tag}A")),
        (other_board, format!("{tag} second"), format!("{tag}B")),
    ] {
        db.exec(
            "INSERT INTO boards (id, tenant_id, workspace_id, name, key) VALUES ($1,$2,$3,$4,$5)",
            params![id, tenant, ws, name, key],
        )
        .await
        .expect("board");
    }
    let column = Uuid::new_v4();
    db.exec(
        "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1,$2,'Todo',0,'unstarted')",
        params![column, board],
    )
    .await
    .expect("column");
    // A column on the OTHER board too, so the indirect scoping has more than
    // one row to reach through.
    db.exec(
        "INSERT INTO board_columns (id, board_id, name, position, type)
         VALUES ($1,$2,'Todo',0,'unstarted')",
        params![Uuid::new_v4(), other_board],
    )
    .await
    .expect("column");

    let (task, blocker) = (Uuid::new_v4(), Uuid::new_v4());
    for (id, title) in [(task, "first"), (blocker, "second")] {
        db.exec(
            "INSERT INTO tasks (id, tenant_id, board_id, column_id, title, description)
             VALUES ($1,$2,$3,$4,$5,'body')",
            params![id, tenant, board, column, format!("{tag} {title}")],
        )
        .await
        .expect("task");
    }
    db.exec(
        "INSERT INTO task_comments (id, tenant_id, task_id, author_type, author_id, body_md)
         VALUES ($1,$2,$3,'user',$4,'a comment')",
        params![Uuid::new_v4(), tenant, task, owner.0],
    )
    .await
    .expect("comment");
    db.exec(
        "INSERT INTO task_relations (id, tenant_id, from_task, to_task, kind)
         VALUES ($1,$2,$3,$4,'blocks')",
        params![Uuid::new_v4(), tenant, blocker, task],
    )
    .await
    .expect("relation");
    db.exec(
        "INSERT INTO task_description_revisions (id, tenant_id, task_id, body, author_id)
         VALUES ($1,$2,$3,'the first body',$4)",
        params![Uuid::new_v4(), tenant, task, owner.0],
    )
    .await
    .expect("revision");
    db.exec(
        "INSERT INTO task_reports (id, tenant_id, task_id, key, title, body_md, author_type)
         VALUES ($1,$2,$3,$4,'a report','findings','agent')",
        params![Uuid::new_v4(), tenant, task, format!("{tag}-report")],
    )
    .await
    .expect("report");

    let label = Uuid::new_v4();
    db.exec(
        "INSERT INTO labels (id, tenant_id, name, color) VALUES ($1,$2,$3,'#fff')",
        params![label, tenant, format!("{tag}-label")],
    )
    .await
    .expect("label");
    db.exec(
        "INSERT INTO task_labels (task_id, label_id) VALUES ($1,$2)",
        params![task, label],
    )
    .await
    .expect("task_label");

    let sha = format!("{:0>64}", format!("{tag}deadbeef"));
    let content = put_content(state, &db, tenant, owner, &sha, b"attachment bytes").await;
    db.exec(
        "INSERT INTO task_attachments (id, tenant_id, user_content_id, parent_kind, parent_id, attached_by)
         VALUES ($1,$2,$3,'task',$4,$5)",
        params![Uuid::new_v4(), tenant, content, task, owner.0],
    )
    .await
    .expect("attachment");

    db.exec(
        "INSERT INTO settings (id, tenant_id, scope, key, value)
         VALUES ($1,$2,'tenant','loops.enabled','true'::jsonb)",
        params![Uuid::new_v4(), tenant],
    )
    .await
    .expect("setting");
    db.exec(
        "INSERT INTO themes (id, tenant_id, name, slug, tokens)
         VALUES ($1,$2,$3,$4,'{}'::jsonb)",
        params![
            Uuid::new_v4(),
            tenant,
            format!("{tag} theme"),
            format!("{tag}-theme")
        ],
    )
    .await
    .expect("theme");
    db.exec(
        "INSERT INTO skills (id, tenant_id, name, content, sha256)
         VALUES ($1,$2,$3,'# a skill','abc')",
        params![Uuid::new_v4(), tenant, format!("{tag}-skill")],
    )
    .await
    .expect("skill");
    db.exec(
        "INSERT INTO notification_channels (id, tenant_id, kind, name, secret)
         VALUES ($1,$2,'webhook',$3,$4)",
        params![
            Uuid::new_v4(),
            tenant,
            format!("{tag}-channel"),
            format!("SECRET-{tag}-CHANNEL")
        ],
    )
    .await
    .expect("channel");

    Seeded { owner, sha }
}

/// A `user_content` row whose bytes really are in the store.
async fn put_content(
    state: &AppState,
    db: &DbPool,
    tenant: TenantId,
    who: UserId,
    sha: &str,
    bytes: &[u8],
) -> Uuid {
    let id = Uuid::new_v4();
    let key = format!("user-content/{id}");
    state
        .user_content_store
        .put(&key, bytes.to_vec())
        .await
        .expect("store the bytes");
    db.exec(
        "INSERT INTO user_content (id, tenant_id, uploaded_by, filename, content_type,
                                   size_bytes, sha256, storage_key)
         VALUES ($1,$2,$3,'a.txt','text/plain',$4,$5,$6)",
        params![
            id,
            tenant,
            who.0,
            bytes.len() as i64,
            sha.to_string(),
            key.clone()
        ],
    )
    .await
    .expect("content row");
    id
}

// ── the archive ─────────────────────────────────────────────────────────────

/// AC-1, AC-3, AC-4, AC-6: the member list is exactly what the format
/// promises, and every manifest count is the database's own answer.
#[tokio::test]
async fn an_archive_carries_the_tenant_and_says_what_it_carried() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if !bed.is_postgres() {
        return;
    }
    let scratch = Scratch::new("whole");
    let state = state_on(&bed, &scratch).await;
    let tenant = bed.tenant("whole").await;
    let s = seed(&bed, &state, tenant, "wh").await;

    let archive = export_ok(&state, ctx(s.owner, tenant), tenant).await;

    let mut want: BTreeSet<String> = fmt::INCLUDED_TABLES
        .iter()
        .map(|t| fmt::table_path(t))
        .collect();
    want.insert("manifest.json".into());
    want.insert(fmt::blob_path(&s.sha));
    assert_eq!(
        archive.paths(),
        want.iter().map(String::as_str).collect::<BTreeSet<_>>(),
        "the archive holds the manifest, one file per included table, and one blob"
    );

    let m = archive.manifest();
    assert_eq!(m["format"], fmt::FORMAT);
    assert_eq!(m["secrets_included"], false);
    assert_eq!(m["tenant"]["id"], tenant.0.to_string());
    assert!(
        m["migrations"]["control"]
            .as_array()
            .is_some_and(|v| !v.is_empty()),
        "the applied control-plane migrations are recorded: {m}"
    );
    assert!(
        m["server_version"].as_str().is_some_and(|v| !v.is_empty()),
        "the server says which version wrote this"
    );
    assert!(
        chrono::DateTime::parse_from_rfc3339(m["exported_at"].as_str().unwrap_or_default()).is_ok(),
        "exported_at is RFC 3339: {}",
        m["exported_at"]
    );

    // Every count in the manifest is the number of lines in its file AND the
    // number of rows in the database.
    for table in fmt::INCLUDED_TABLES {
        let claimed = m["tables"][*table].as_i64().unwrap_or_else(|| {
            panic!("the manifest has no count for {table}: {m}");
        });
        assert_eq!(
            claimed,
            archive.rows(table).len() as i64,
            "{table}: the manifest count and the file disagree"
        );
        let scope = fmt::scope_sql(table);
        let actual = count(
            &bed.db(),
            &format!("SELECT count(*) AS n FROM {table} t WHERE {scope}"),
            tenant,
        )
        .await;
        assert_eq!(
            claimed, actual,
            "{table}: the manifest disagrees with the database"
        );
    }
    assert_eq!(m["blobs"]["count"], 1);
    assert_eq!(m["blobs"]["bytes"], b"attachment bytes".len());

    // Two boards, and both of their columns came through the indirect scope.
    assert_eq!(archive.rows("boards").len(), 2);
    assert_eq!(archive.rows("board_columns").len(), 2);
    assert_eq!(archive.rows("task_labels").len(), 1);
    let titles: Vec<String> = archive
        .rows("tasks")
        .iter()
        .filter_map(|r| r["title"].as_str().map(str::to_string))
        .collect();
    assert!(titles.iter().any(|t| t.contains("first")), "{titles:?}");

    assert_eq!(archive.text(&fmt::blob_path(&s.sha)), "attachment bytes");
    bed.teardown().await;
}

/// AC-5: an export of one tenant contains no row belonging to another, for
/// every table the archive carries.
#[tokio::test]
async fn no_row_of_another_tenant_travels() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if !bed.is_postgres() {
        return;
    }
    let scratch = Scratch::new("iso");
    let state = state_on(&bed, &scratch).await;
    let mine = bed.tenant("mine").await;
    let theirs = bed.tenant("theirs").await;
    let s = seed(&bed, &state, mine, "mi").await;
    seed(&bed, &state, theirs, "th").await;

    // A GLOBAL theme belongs to the deployment, not to either tenant (AC-5).
    bed.db()
        .exec(
            "INSERT INTO themes (id, tenant_id, name, slug, tokens)
             VALUES ($1, NULL, 'Global', 'global-theme', '{}'::jsonb)",
            params![Uuid::new_v4()],
        )
        .await
        .expect("global theme");

    let archive = export_ok(&state, ctx(s.owner, mine), mine).await;

    let theirs_id = theirs.0.to_string();
    for table in fmt::INCLUDED_TABLES {
        for row in archive.rows(table) {
            let text = row.to_string();
            assert!(
                !text.contains(&theirs_id),
                "{table} carried a row naming the other tenant: {text}"
            );
        }
    }
    // `tenants` itself is one row: the tenant being exported.
    let tenants = archive.rows("tenants");
    assert_eq!(tenants.len(), 1);
    assert_eq!(tenants[0]["id"], mine.0.to_string());

    // The global theme is not the tenant's to take.
    for row in archive.rows("themes") {
        assert_eq!(row["tenant_id"], mine.0.to_string(), "{row}");
    }
    assert_eq!(archive.rows("themes").len(), 1);

    // And no blob of theirs.
    assert_eq!(archive.manifest()["blobs"]["count"], 1);
    bed.teardown().await;
}

/// AC-2: owner-only, and never a machine.
#[tokio::test]
async fn only_this_tenants_owner_may_export() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if !bed.is_postgres() {
        return;
    }
    let scratch = Scratch::new("gate");
    let state = state_on(&bed, &scratch).await;
    let tenant = bed.tenant("gate").await;
    let owner = member(&bed, tenant, "owner").await;
    let admin = member(&bed, tenant, "admin").await;
    let plain = member(&bed, tenant, "member").await;
    let stranger_tenant = bed.tenant("stranger").await;
    let stranger = member(&bed, stranger_tenant, "owner").await;
    let (person, _) = bed.user(tenant, "owner").await;
    let node = bed.node(tenant, person.0).await;

    for (who, label) in [
        (ctx(admin, tenant), "an admin"),
        (ctx(plain, tenant), "a member"),
        (node_ctx(node, owner, tenant), "a node token"),
    ] {
        assert_eq!(
            export_status(&state, who, tenant).await,
            StatusCode::FORBIDDEN,
            "{label} cannot export a tenant"
        );
    }
    // Another tenant's owner is refused before membership is even consulted:
    // you export the tenant you are switched into.
    assert_eq!(
        export_status(&state, ctx(stranger, stranger_tenant), tenant).await,
        StatusCode::FORBIDDEN,
        "another tenant's owner cannot export this one"
    );
    // And the owner can.
    let archive = export_ok(&state, ctx(owner, tenant), tenant).await;
    assert_eq!(archive.manifest()["tenant"]["id"], tenant.0.to_string());
    bed.teardown().await;
}

/// AC-6: content is stored by digest, so the same bytes uploaded twice are one
/// entry — and every exported row's bytes are present.
#[tokio::test]
async fn one_digest_is_one_entry_and_every_row_has_its_bytes() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if !bed.is_postgres() {
        return;
    }
    let scratch = Scratch::new("blobs");
    let state = state_on(&bed, &scratch).await;
    let tenant = bed.tenant("blobs").await;
    let owner = member(&bed, tenant, "owner").await;

    let shared = format!("{:0>64}", "aa");
    let alone = format!("{:0>64}", "bb");
    put_content(&state, &bed.db(), tenant, owner, &shared, b"same bytes").await;
    put_content(&state, &bed.db(), tenant, owner, &shared, b"same bytes").await;
    put_content(&state, &bed.db(), tenant, owner, &alone, b"other bytes").await;

    let archive = export_ok(&state, ctx(owner, tenant), tenant).await;

    let blobs: BTreeSet<&str> = archive
        .paths()
        .into_iter()
        .filter(|p| p.starts_with("content/"))
        .collect();
    assert_eq!(
        blobs.len(),
        2,
        "three rows, two distinct digests, two entries: {blobs:?}"
    );
    assert_eq!(archive.manifest()["blobs"]["count"], 2);
    assert_eq!(
        archive.manifest()["blobs"]["bytes"],
        (b"same bytes".len() + b"other bytes".len())
    );

    // Every row points at a digest the archive has.
    let rows = archive.rows("user_content");
    assert_eq!(rows.len(), 3);
    for row in rows {
        let sha = row["sha256"].as_str().expect("a digest");
        assert!(
            blobs.contains(fmt::blob_path(sha).as_str()),
            "no bytes for {sha}: {blobs:?}"
        );
    }
    assert_eq!(archive.text(&fmt::blob_path(&shared)), "same bytes");
    bed.teardown().await;
}

/// AC-7 and NG-2: a secret value is a null with a marker beside it, and no
/// secret byte appears anywhere in the archive.
#[tokio::test]
async fn a_secret_value_never_travels() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if !bed.is_postgres() {
        return;
    }
    let scratch = Scratch::new("secret");
    let state = state_on(&bed, &scratch).await;
    let tenant = bed.tenant("secret").await;
    let s = seed(&bed, &state, tenant, "se").await;

    // The two secret-bearing columns on tables that DO travel, filled in.
    bed.db()
        .exec(
            "UPDATE workspaces SET gh_token_enc = $2, webhook_secret_enc = $3 WHERE tenant_id = $1",
            params![
                tenant,
                b"GH-TOKEN-CIPHERTEXT".to_vec(),
                b"WEBHOOK-CIPHERTEXT".to_vec()
            ],
        )
        .await
        .expect("seal something");
    bed.db()
        .exec(
            "UPDATE users SET password_hash = 'PASSWORD-HASH-SECRET' WHERE tenant_id = $1",
            params![tenant],
        )
        .await
        .expect("a password hash");
    // And the two whose whole TABLE is excluded, to prove the archive has no
    // file for them at all.
    let cred = Uuid::new_v4();
    bed.db()
        .exec(
            "INSERT INTO git_credentials (id, tenant_id, name, secret_enc)
             VALUES ($1,$2,'a key',$3)",
            params![cred, tenant, b"GIT-CREDENTIAL-CIPHERTEXT".to_vec()],
        )
        .await
        .expect("a git credential");

    let archive = export_ok(&state, ctx(s.owner, tenant), tenant).await;

    let channel = &archive.rows("notification_channels")[0];
    assert!(channel["secret"].is_null(), "{channel}");
    assert_eq!(channel[fmt::VALUE_OMITTED_KEY], true, "{channel}");
    assert!(
        channel["name"]
            .as_str()
            .is_some_and(|n| n.contains("se-channel")),
        "the rest of the row still travels: {channel}"
    );

    let workspace = &archive.rows("workspaces")[0];
    assert!(workspace["gh_token_enc"].is_null(), "{workspace}");
    assert!(workspace["webhook_secret_enc"].is_null(), "{workspace}");
    assert_eq!(workspace[fmt::VALUE_OMITTED_KEY], true, "{workspace}");
    assert!(
        workspace["slug"].as_str().is_some_and(|s| !s.is_empty()),
        "{workspace}"
    );

    for row in archive.rows("users") {
        assert!(row["password_hash"].is_null(), "{row}");
        assert_eq!(row[fmt::VALUE_OMITTED_KEY], true, "{row}");
    }

    // NG-4 / AC-4: the wholly-excluded tables have no file.
    for table in [
        "git_credentials",
        "workspace_secrets",
        "nodes",
        "user_tokens",
    ] {
        assert!(
            !archive.paths().contains(fmt::table_path(table).as_str()),
            "{table} must not be in the archive"
        );
    }

    let all = archive.every_byte();
    for secret in [
        "SECRET-se-CHANNEL",
        "GH-TOKEN-CIPHERTEXT",
        "WEBHOOK-CIPHERTEXT",
        "PASSWORD-HASH-SECRET",
        "GIT-CREDENTIAL-CIPHERTEXT",
    ] {
        assert!(
            !contains(&all, secret.as_bytes()),
            "{secret} appears somewhere in the archive"
        );
        // Base64 too — a bytea column that slipped through would be encoded.
        let encoded = base64_of(secret.as_bytes());
        assert!(
            !contains(&all, encoded.as_bytes()),
            "{secret} appears base64-encoded in the archive"
        );
    }
    bed.teardown().await;
}

/// AC-4: a table with no rows still gets its (empty) file, so a reader never
/// has to tell "no rows" from "not exported".
#[tokio::test]
async fn an_empty_tenant_still_gets_every_file() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if !bed.is_postgres() {
        return;
    }
    let scratch = Scratch::new("empty");
    let state = state_on(&bed, &scratch).await;
    let tenant = bed.tenant("empty").await;
    let owner = member(&bed, tenant, "owner").await;

    let archive = export_ok(&state, ctx(owner, tenant), tenant).await;

    for table in fmt::INCLUDED_TABLES {
        let path = fmt::table_path(table);
        assert!(
            archive.paths().contains(path.as_str()),
            "{path} is missing from an empty tenant's archive"
        );
    }
    assert_eq!(archive.rows("tasks").len(), 0);
    assert_eq!(archive.text(&fmt::table_path("tasks")), "");
    assert_eq!(archive.manifest()["tables"]["tasks"], 0);
    assert_eq!(archive.manifest()["blobs"]["count"], 0);
    // The tenant, its owner and the membership grant are the rows it does have.
    assert_eq!(archive.rows("tenants").len(), 1);
    assert_eq!(archive.rows("tenant_members").len(), 1);
    bed.teardown().await;
}

// ── the drift guards, against the live schema ───────────────────────────────

/// Read the migrated schema the same way the export does.
async fn live_columns(db: &DbPool, table: &str) -> Vec<Column> {
    #[derive(nook_db::FromDbRow)]
    struct Row {
        column_name: String,
        udt_name: String,
    }
    let rows: Vec<Row> = db
        .query_all(
            "SELECT column_name, udt_name FROM information_schema.columns
              WHERE table_schema = 'public' AND table_name = $1
              ORDER BY ordinal_position",
            params![table.to_string()],
        )
        .await
        .expect("columns");
    rows.into_iter()
        .map(|r| Column::new(r.column_name, r.udt_name))
        .collect()
}

async fn live_scoped_tables(db: &DbPool) -> BTreeSet<String> {
    #[derive(nook_db::FromDbRow)]
    struct Row {
        name: String,
    }
    let rows: Vec<Row> = db
        .query_all(
            "SELECT DISTINCT c.table_name AS name
               FROM information_schema.columns c
               JOIN information_schema.tables t
                 ON t.table_schema = c.table_schema AND t.table_name = c.table_name
              WHERE c.table_schema = 'public'
                AND c.column_name = 'tenant_id'
                AND t.table_type = 'BASE TABLE'",
            params![],
        )
        .await
        .expect("scoped tables");
    let mut names: BTreeSet<String> = rows.into_iter().map(|r| r.name).collect();
    for extra in fmt::INDIRECTLY_SCOPED_TABLES {
        names.insert((*extra).to_string());
    }
    names
}

/// AC-11 against the real schema: nothing tenant-scoped is unclassified.
#[tokio::test]
async fn every_tenant_scoped_table_is_classified() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if !bed.is_postgres() {
        return;
    }
    let scoped = live_scoped_tables(&bed.db()).await;
    assert!(
        scoped.len() > 20,
        "the schema read found almost nothing — the guard would pass vacuously: {scoped:?}"
    );
    let problems = fmt::classification_drift(&scoped);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    bed.teardown().await;
}

/// AC-12 against the real schema: what the export writes for each included
/// table is exactly that table's columns.
#[tokio::test]
async fn every_column_of_an_included_table_travels() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if !bed.is_postgres() {
        return;
    }
    for table in fmt::INCLUDED_TABLES {
        let columns = live_columns(&bed.db(), table).await;
        assert!(!columns.is_empty(), "{table} is not in this schema");
        let schema: BTreeSet<String> = columns.iter().map(|c| c.name.clone()).collect();
        let exported = fmt::exported_keys(table, &columns);
        assert!(
            fmt::column_drift(table, &exported, &schema).is_none(),
            "{}",
            fmt::column_drift(table, &exported, &schema).unwrap_or_default()
        );
    }
    bed.teardown().await;
}

/// AC-13 against the real schema: no included table has a secret-shaped column
/// that is neither declared nor acknowledged.
#[tokio::test]
async fn no_included_table_leaks_a_secret_shaped_column() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if !bed.is_postgres() {
        return;
    }
    let mut problems = Vec::new();
    for table in fmt::INCLUDED_TABLES {
        let columns = live_columns(&bed.db(), table).await;
        problems.extend(fmt::secret_shape_drift(table, &columns));
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    bed.teardown().await;
}

/// AC-12's live check exists to catch a column the export stops carrying, so
/// prove it can: feed the guard a real table's columns with one removed.
#[tokio::test]
async fn the_live_column_guard_catches_a_dropped_column() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if !bed.is_postgres() {
        return;
    }
    let columns = live_columns(&bed.db(), "tasks").await;
    let schema: BTreeSet<String> = columns.iter().map(|c| c.name.clone()).collect();
    let mut truncated = columns.clone();
    let dropped = truncated.pop().expect("tasks has columns").name;
    let exported = fmt::exported_keys("tasks", &truncated);

    let msg = fmt::column_drift("tasks", &exported, &schema)
        .expect("a column silently stopping travelling must fail the build");
    assert!(msg.contains(&dropped), "{msg}");
    bed.teardown().await;
}

/// AC-13's live check the same way: a `secret_enc` on a real included table's
/// column list must be refused.
#[tokio::test]
async fn the_live_secret_guard_catches_a_new_secret_column() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    if !bed.is_postgres() {
        return;
    }
    let mut columns = live_columns(&bed.db(), "tasks").await;
    columns.push(Column::new("secret_enc", "bytea"));
    let problems = fmt::secret_shape_drift("tasks", &columns);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("secret_enc"), "{}", problems[0]);
    bed.teardown().await;
}

// ── small helpers ───────────────────────────────────────────────────────────

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
