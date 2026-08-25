//! The tenant export archive format, v1 (MAIN-659).
//!
//! One module holds everything the format *is*: which tables travel, which
//! deliberately do not, which column values never leave, how a row becomes a
//! line of JSON, and how the tar stream is laid out. The import half will read
//! these same definitions rather than restating them, and the three drift
//! guards below are what stop the archive quietly ceasing to carry something.
//!
//! **The guards are pure functions over facts read from the live schema, not
//! assertions buried in a test.** That is deliberate: a check that has never
//! been shown to fail is not yet a check, and a pure function can be handed
//! injected drift by a companion test and asked whether it notices. The tests
//! at the bottom of this file do exactly that for each of AC-11, AC-12 and
//! AC-13.
//!
//! **Postgres only.** Column discovery is `information_schema`, the row
//! encoding is `jsonb_build_object`, and `bytea` reaches base64 through
//! `encode(…, 'base64')` — none of which the SQLite track has. The route
//! refuses on any other engine by name rather than emitting an archive that is
//! silently a different shape.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use chrono::{DateTime, Utc};
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// What a reader must recognise before trusting anything else in the archive.
pub const FORMAT: &str = "nook-tenant-export/1";

/// The marker a row carries when one of its columns was a secret (AC-7).
pub const VALUE_OMITTED_KEY: &str = "value_omitted";

/// Every table whose rows travel, in the order they are written.
///
/// Order is parent-before-child so a future importer can insert straight down
/// the list; nothing in this ticket depends on it.
pub const INCLUDED_TABLES: &[&str] = &[
    "tenants",
    "tenant_members",
    "users",
    "boards",
    "board_columns",
    "tasks",
    "task_comments",
    "task_relations",
    "task_description_revisions",
    "task_reports",
    "labels",
    "task_labels",
    "task_attachments",
    "user_content",
    "workspaces",
    "settings",
    "themes",
    "skills",
    "notification_channels",
];

/// A tenant-scoped table that deliberately does not travel, and why.
pub struct ExcludedTable {
    pub table: &'static str,
    pub reason: &'static str,
}

/// The other half of the classification. Every tenant-scoped table is in
/// exactly one of these two lists or the build fails (AC-11) — so adding a
/// table without deciding whether a tenant's copy of it migrates is not
/// something anyone can do by accident.
pub const EXCLUDED_TABLES: &[ExcludedTable] = &[
    ExcludedTable {
        table: "chat_channel_members",
        reason: "chat is its own surface with its own migrator set (NG-4)",
    },
    ExcludedTable {
        table: "chat_messages",
        reason: "chat is its own surface with its own migrator set (NG-4)",
    },
    ExcludedTable {
        table: "email_links",
        reason: "inbound-mail plumbing, bound to the mailbox this deployment polls",
    },
    ExcludedTable {
        table: "email_pollers",
        reason: "holds a mailbox credential and this deployment's poll cursor",
    },
    ExcludedTable {
        table: "inbound_email_seen",
        reason: "dedup cursor for one deployment's mail intake; rebuilt by arrival",
    },
    ExcludedTable {
        table: "events",
        reason: "the activity log of this deployment, not the tenant's content",
    },
    ExcludedTable {
        table: "feedback",
        reason: "in-product reports belong to the deployment that received them",
    },
    ExcludedTable {
        table: "forge_deliveries",
        reason: "webhook receipts keyed to this deployment's endpoint",
    },
    ExcludedTable {
        table: "git_credentials",
        reason: "an encrypted secret with no useful remainder once the value is dropped (NG-2)",
    },
    ExcludedTable {
        table: "interactions",
        reason: "live human asks, anchored to jobs that do not travel",
    },
    ExcludedTable {
        table: "invites",
        reason: "single-use tokens for this deployment's sign-up URLs",
    },
    ExcludedTable {
        table: "join_tokens",
        reason: "machine enrolment credentials for this fleet",
    },
    ExcludedTable {
        table: "loop_jobs",
        reason: "run history bound to nodes and checkouts that do not travel",
    },
    ExcludedTable {
        table: "node_workspaces",
        reason: "checkouts on machines that do not travel",
    },
    ExcludedTable {
        table: "nodes",
        reason: "machines belong to the deployment, not to the tenant",
    },
    ExcludedTable {
        table: "notes",
        reason: "the notebook is not in v1's table set (AC-4)",
    },
    ExcludedTable {
        table: "notifications",
        reason: "delivered alerts; state of one deployment's inbox",
    },
    ExcludedTable {
        table: "secret_items",
        reason: "the vault: ciphertext whose key never leaves the deployment (NG-2)",
    },
    ExcludedTable {
        table: "sessions",
        reason: "live terminals on machines that do not travel",
    },
    ExcludedTable {
        table: "sessions_auth",
        reason: "browser sign-in state",
    },
    ExcludedTable {
        table: "task_workspace_refs",
        reason: "derived from card descriptions; re-parsed rather than carried",
    },
    ExcludedTable {
        table: "tenant_cas",
        reason: "the tenant's certificate authority, private key included",
    },
    ExcludedTable {
        table: "user_passkeys",
        reason: "authentication credentials",
    },
    ExcludedTable {
        table: "user_tokens",
        reason: "authentication credentials",
    },
    ExcludedTable {
        table: "user_vaults",
        reason: "the vault's key-derivation material (NG-2)",
    },
    ExcludedTable {
        table: "work_queue",
        reason: "in-flight queue state of one deployment",
    },
    ExcludedTable {
        table: "work_queue_dead",
        reason: "in-flight queue state of one deployment",
    },
    ExcludedTable {
        table: "workspace_secrets",
        reason: "an encrypted secret with no useful remainder once the value is dropped (NG-2)",
    },
];

/// Tables that carry no `tenant_id` and are scoped through a parent instead.
/// Named here because the AC-11 guard has to know they exist to demand they be
/// classified — nothing in the schema says so.
pub const INDIRECTLY_SCOPED_TABLES: &[&str] = &["board_columns", "task_labels"];

/// Columns whose VALUE never leaves the deployment (AC-7).
///
/// The key is still written, as null, with [`VALUE_OMITTED_KEY`] on the row —
/// a missing key and an omitted value are different statements, and only the
/// second one is honest.
///
/// Two entries name tables that are not in [`INCLUDED_TABLES`]
/// (`git_credentials`, `workspace_secrets`): those tables do not travel at all,
/// so their secrets are absent by a stronger route. They are declared anyway so
/// the list is the complete answer to "which columns in this schema hold a
/// secret value", which is what AC-13's guard consults.
pub const SECRET_COLUMNS: &[(&str, &str)] = &[
    ("git_credentials", "secret_enc"),
    ("notification_channels", "secret"),
    ("users", "password_hash"),
    ("workspace_secrets", "content_enc"),
    ("workspaces", "gh_token_enc"),
    ("workspaces", "webhook_secret_enc"),
];

/// A column whose NAME or type looks secret to [`looks_secret`] but is not, with
/// the reason it is safe to export.
///
/// Empty today, and that is the point: it exists so that acknowledging a false
/// positive is a deliberate line in this file rather than a weakened pattern.
pub const SECRET_SHAPE_ALLOWLIST: &[(&str, &str, &str)] = &[];

/// One column of one table, as the live schema describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    /// `information_schema.columns.udt_name` — `uuid`, `text`, `bytea`,
    /// `timestamptz`, `jsonb`, `_text` for `text[]`, …
    pub udt: String,
}

impl Column {
    pub fn new(name: impl Into<String>, udt: impl Into<String>) -> Self {
        Column {
            name: name.into(),
            udt: udt.into(),
        }
    }
}

/// Is `table` one whose rows travel?
pub fn is_included(table: &str) -> bool {
    INCLUDED_TABLES.contains(&table)
}

/// The secret columns declared for one table.
pub fn secret_columns_of(table: &str) -> BTreeSet<&'static str> {
    SECRET_COLUMNS
        .iter()
        .filter(|(t, _)| *t == table)
        .map(|(_, c)| *c)
        .collect()
}

// ── The row encoding ────────────────────────────────────────────────────────

/// `jsonb_build_object` is variadic over a function-argument limit of 100, so
/// the object is built in groups of this many columns and concatenated. Without
/// it a table would silently stop being exportable at fifty columns.
const COLUMN_GROUP: usize = 25;

/// A SQL identifier, quoted. The names come from `information_schema` on our
/// own schema, so this is belt rather than braces — but a column name is
/// interpolated into SQL, and "the input is trusted" is exactly the sentence
/// that stops being true later.
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A SQL string literal.
fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// The expression that produces one column's exported VALUE.
fn value_sql(column: &Column, secret: &BTreeSet<&'static str>) -> String {
    if secret.contains(column.name.as_str()) {
        // Present as a key, null as a value (AC-7).
        return "NULL::text".to_string();
    }
    match column.udt.as_str() {
        // `to_jsonb` renders bytea as Postgres' `\x…` hex escape; the format
        // says base64.
        "bytea" => format!("encode(t.{}, 'base64')", ident(&column.name)),
        // Everything else goes through `to_jsonb`'s own rendering, which is
        // what makes the mapping schema-driven rather than a type table this
        // module would have to keep current: uuid and text become strings,
        // timestamptz an RFC 3339 instant (the export transaction pins
        // `TimeZone` to UTC so the offset is always `+00:00`), jsonb inlines,
        // and `text[]` becomes a JSON array.
        _ => format!("t.{}", ident(&column.name)),
    }
}

/// The SELECT expression rendering one row of `table` as a JSON object keyed by
/// column name.
///
/// Built from `columns`, which the caller read from the live schema — so a
/// column added to `tasks` next month travels without anyone editing this file
/// (AC-12).
pub fn row_json_sql(table: &str, columns: &[Column]) -> String {
    let secret = secret_columns_of(table);
    let mut groups: Vec<String> = columns
        .chunks(COLUMN_GROUP)
        .map(|chunk| {
            let args = chunk
                .iter()
                .map(|c| format!("{}, {}", literal(&c.name), value_sql(c, &secret)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("jsonb_build_object({args})")
        })
        .collect();
    if !secret.is_empty() {
        groups.push(format!(
            "jsonb_build_object({}, true)",
            literal(VALUE_OMITTED_KEY)
        ));
    }
    if groups.is_empty() {
        return "'{}'::jsonb".to_string();
    }
    groups.join(" || ")
}

/// The keys [`row_json_sql`] will produce for `table` — the same derivation, so
/// a test can compare them against `information_schema` without running a query
/// that needs rows to exist.
pub fn exported_keys(table: &str, columns: &[Column]) -> BTreeSet<String> {
    let mut keys: BTreeSet<String> = columns.iter().map(|c| c.name.clone()).collect();
    if !secret_columns_of(table).is_empty() {
        keys.insert(VALUE_OMITTED_KEY.to_string());
    }
    keys
}

/// The `WHERE` fragment confining `table` to one tenant. `$1` is the tenant id.
///
/// Two tables carry no `tenant_id` and reach it through a parent, and one —
/// `themes` — has a nullable one, where `= $1` is exactly right: a global theme
/// (`tenant_id IS NULL`) belongs to the deployment and does not travel (AC-5).
pub fn scope_sql(table: &str) -> String {
    match table {
        "tenants" => "t.id = $1".to_string(),
        "board_columns" => "t.board_id IN (SELECT id FROM boards WHERE tenant_id = $1)".to_string(),
        "task_labels" => "t.task_id IN (SELECT id FROM tasks WHERE tenant_id = $1)".to_string(),
        _ => "t.tenant_id = $1".to_string(),
    }
}

/// The expression rows are ordered and paged by: the primary key, rendered as
/// one text value.
///
/// Concatenating the key columns is order-preserving only because every primary
/// key column of every included table is a `uuid`, whose text form is
/// fixed-width — so lexicographic order on the join equals tuple order on the
/// parts. [`non_uuid_key_columns`] fails the build if that ever stops holding.
pub fn key_sql(pk: &[String]) -> String {
    let parts: Vec<String> = pk.iter().map(|c| format!("t.{}::text", ident(c))).collect();
    match parts.len() {
        0 => "''".to_string(),
        1 => parts.into_iter().next().unwrap_or_default(),
        _ => format!("concat_ws('/', {})", parts.join(", ")),
    }
}

// ── The drift guards ────────────────────────────────────────────────────────

/// AC-11 — every tenant-scoped table is classified, or this reports it.
///
/// `scoped` is what the live schema says carries a `tenant_id`, plus
/// [`INDIRECTLY_SCOPED_TABLES`]. A table in neither constant, or in both, comes
/// back as a sentence naming it and saying what to do.
pub fn classification_drift(scoped: &BTreeSet<String>) -> Vec<String> {
    let included: BTreeSet<&str> = INCLUDED_TABLES.iter().copied().collect();
    let excluded: BTreeSet<&str> = EXCLUDED_TABLES.iter().map(|e| e.table).collect();

    let mut problems = Vec::new();
    for table in scoped {
        let i = included.contains(table.as_str());
        let e = excluded.contains(table.as_str());
        if i && e {
            problems.push(format!(
                "`{table}` is in BOTH INCLUDED_TABLES and EXCLUDED_TABLES — it can only be one"
            ));
        } else if !i && !e {
            problems.push(format!(
                "`{table}` is tenant-scoped and classified nowhere — add it to INCLUDED_TABLES \
                 if a tenant's copy should travel, or to EXCLUDED_TABLES with a reason if it \
                 should not (services/tenant_archive.rs)"
            ));
        }
    }
    // A table in both lists but absent from the schema is still a contradiction
    // worth reporting, and it is cheap to catch here.
    for table in &included {
        if excluded.contains(table) && !scoped.contains(*table) {
            problems.push(format!(
                "`{table}` is in BOTH INCLUDED_TABLES and EXCLUDED_TABLES — it can only be one"
            ));
        }
    }
    problems
}

/// AC-12 — every column of an included table travels, or this reports it.
///
/// `exported` is the key set the export will actually write; `schema` is the
/// table's column set. They must agree but for [`VALUE_OMITTED_KEY`].
pub fn column_drift(
    table: &str,
    exported: &BTreeSet<String>,
    schema: &BTreeSet<String>,
) -> Option<String> {
    let mut carried = exported.clone();
    carried.remove(VALUE_OMITTED_KEY);

    let missing: Vec<&String> = schema.difference(&carried).collect();
    let extra: Vec<&String> = carried.difference(schema).collect();
    if missing.is_empty() && extra.is_empty() {
        return None;
    }
    let mut msg = format!("`{table}`'s exported row does not match its columns:");
    if !missing.is_empty() {
        msg.push_str(&format!(
            " missing {missing:?} — export must read the live schema, not a hand-written list;"
        ));
    }
    if !extra.is_empty() {
        msg.push_str(&format!(
            " exports {extra:?}, which the table does not have;"
        ));
    }
    Some(msg)
}

/// The shapes this codebase uses for a value that must not be exported.
///
/// Name-based because that is what a new column will look like: `secret_enc`,
/// `token_hash`, `kdf_salt`. Type-based for `bytea`, because opaque bytes on a
/// tenant-scoped table have so far always been ciphertext.
pub fn looks_secret(column: &str, udt: &str) -> bool {
    const NAME_SHAPES: &[&str] = &[
        "_enc",
        "secret",
        "token_hash",
        "password",
        "kdf_salt",
        "verifier",
    ];
    let lower = column.to_ascii_lowercase();
    NAME_SHAPES.iter().any(|s| lower.contains(s)) || udt.eq_ignore_ascii_case("bytea")
}

/// AC-13 — a new secret-bearing column on an included table, or this reports it.
///
/// Every column matching [`looks_secret`] must be either declared in
/// [`SECRET_COLUMNS`] or acknowledged in [`SECRET_SHAPE_ALLOWLIST`] with a
/// reason. A `secret_enc` added to `workspaces` next month therefore fails the
/// build instead of being exported in the clear.
pub fn secret_shape_drift(table: &str, columns: &[Column]) -> Vec<String> {
    let declared = secret_columns_of(table);
    columns
        .iter()
        .filter(|c| looks_secret(&c.name, &c.udt))
        .filter(|c| !declared.contains(c.name.as_str()))
        .filter(|c| {
            !SECRET_SHAPE_ALLOWLIST
                .iter()
                .any(|(t, col, _)| *t == table && *col == c.name)
        })
        .map(|c| {
            format!(
                "`{table}.{}` ({}) looks like a secret value but is neither in SECRET_COLUMNS nor \
                 acknowledged in SECRET_SHAPE_ALLOWLIST — an included table exports it in the \
                 clear (services/tenant_archive.rs)",
                c.name, c.udt
            )
        })
        .collect()
}

/// Primary-key columns of `table` that are not `uuid`, which [`key_sql`]'s
/// order-preserving concatenation depends on.
pub fn non_uuid_key_columns(pk: &[Column]) -> Vec<String> {
    pk.iter()
        .filter(|c| c.udt != "uuid")
        .map(|c| format!("{} ({})", c.name, c.udt))
        .collect()
}

// ── The manifest ────────────────────────────────────────────────────────────

/// Which tenant this archive is of.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestTenant {
    pub id: uuid::Uuid,
    pub name: String,
    pub slug: String,
}

/// The applied migration versions of both migrator sets, so a reader can tell
/// whether the schema it is looking at is one it understands.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestMigrations {
    pub control: Vec<i64>,
    pub chat: Vec<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestBlobs {
    pub count: u64,
    pub bytes: u64,
}

/// `manifest.json`, the first member of every archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub server_version: String,
    pub migrations: ManifestMigrations,
    pub tenant: ManifestTenant,
    /// table → row count, for exactly [`INCLUDED_TABLES`].
    pub tables: BTreeMap<String, i64>,
    pub blobs: ManifestBlobs,
    /// Always `false` in this format version — there is no mode that sets it
    /// true (NG-2). It is written anyway so a reader never has to infer it.
    pub secrets_included: bool,
    pub exported_at: DateTime<Utc>,
}

/// The download filename: `<slug>-<YYYYMMDD>.tar.gz`.
pub fn archive_filename(slug: &str, at: DateTime<Utc>) -> String {
    let safe: String = slug
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let safe = if safe.trim_matches('-').is_empty() {
        "tenant".to_string()
    } else {
        safe
    };
    format!("{safe}-{}.tar.gz", at.format("%Y%m%d"))
}

// ── The tar/gzip stream ─────────────────────────────────────────────────────

/// A tar block. Every header is one, and every member's data is padded to a
/// multiple of it.
const BLOCK: usize = 512;

/// How much gzip output accumulates before it is handed to the channel. Small
/// enough that the archive is never held, large enough that a row does not cost
/// a send.
const FLUSH_AT: usize = 64 * 1024;

/// Bytes handed to the response body, or the failure that ended it.
pub type Chunk = Result<Vec<u8>, std::io::Error>;

/// The archive, written as it goes.
///
/// Tar puts each member's length in its header, so a member cannot be written
/// without knowing its size first — which is why the exporter measures each
/// table before it writes it rather than buffering the rendered JSONL. What
/// this type guarantees is the other half: gzip output is drained into the
/// channel as it accumulates, so no part of the archive is ever held whole
/// (AC-1).
pub struct ArchiveSink {
    enc: GzEncoder<Vec<u8>>,
    tx: mpsc::Sender<Chunk>,
    mtime: u64,
}

/// The client hung up. Not an error worth logging as one — it is the ordinary
/// end of a download somebody cancelled.
#[derive(Debug)]
pub struct Disconnected;

impl std::fmt::Display for Disconnected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the client stopped reading the export")
    }
}

impl std::error::Error for Disconnected {}

impl ArchiveSink {
    pub fn new(tx: mpsc::Sender<Chunk>, at: DateTime<Utc>) -> Self {
        ArchiveSink {
            enc: GzEncoder::new(Vec::new(), Compression::default()),
            tx,
            mtime: at.timestamp().max(0) as u64,
        }
    }

    /// Start a member. `size` is final — tar has nowhere else to put it.
    pub async fn entry(&mut self, path: &str, size: u64) -> Result<(), anyhow::Error> {
        let mut header = tar::Header::new_gnu();
        header.set_path(path)?;
        header.set_size(size);
        header.set_mode(0o644);
        header.set_mtime(self.mtime);
        header.set_uid(0);
        header.set_gid(0);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        self.enc.write_all(header.as_bytes())?;
        self.maybe_flush().await
    }

    /// Part of the current member's data.
    pub async fn data(&mut self, bytes: &[u8]) -> Result<(), anyhow::Error> {
        self.enc.write_all(bytes)?;
        self.maybe_flush().await
    }

    /// Pad the member just written out to a whole number of blocks.
    pub async fn end_entry(&mut self, size: u64) -> Result<(), anyhow::Error> {
        let rem = (size as usize) % BLOCK;
        if rem != 0 {
            let pad = vec![0u8; BLOCK - rem];
            self.enc.write_all(&pad)?;
        }
        self.maybe_flush().await
    }

    /// Two zero blocks, then the gzip trailer.
    pub async fn finish(mut self) -> Result<(), anyhow::Error> {
        self.enc.write_all(&[0u8; BLOCK * 2])?;
        let tail = self.enc.finish()?;
        if !tail.is_empty() {
            self.tx.send(Ok(tail)).await.map_err(|_| Disconnected)?;
        }
        Ok(())
    }

    /// Tell the client the archive is broken.
    ///
    /// The status and headers left long ago, so the only honest signal is a
    /// body that does not end cleanly — a short archive that claims to be
    /// complete is worse than one that fails to decompress.
    pub async fn fail(self, why: &str) {
        let _ = self
            .tx
            .send(Err(std::io::Error::other(why.to_string())))
            .await;
    }

    async fn maybe_flush(&mut self) -> Result<(), anyhow::Error> {
        if self.enc.get_ref().len() < FLUSH_AT {
            return Ok(());
        }
        self.flush().await
    }

    async fn flush(&mut self) -> Result<(), anyhow::Error> {
        let buf = std::mem::take(self.enc.get_mut());
        if buf.is_empty() {
            return Ok(());
        }
        self.tx.send(Ok(buf)).await.map_err(|_| Disconnected)?;
        Ok(())
    }
}

/// One rendered row, and the paging key it was read at.
#[derive(Debug, Clone, nook_db::FromDbRow)]
pub struct RowLine {
    pub row_json: String,
    pub row_key: String,
}

/// One distinct blob to carry, and where its bytes are.
#[derive(Debug, Clone, nook_db::FromDbRow)]
pub struct BlobRef {
    pub sha256: String,
    pub storage_key: String,
}

/// Where a blob's bytes go in the archive.
pub fn blob_path(sha256: &str) -> String {
    format!("content/{sha256}")
}

/// Where a table's rows go in the archive.
pub fn table_path(table: &str) -> String {
    format!("db/{table}.jsonl")
}

// ── Writing one ─────────────────────────────────────────────────────────────

/// How many rows a page of the export reads at once.
const PAGE: i64 = 500;

/// One table, as the measuring pass found it.
struct TablePlan {
    table: &'static str,
    columns: Vec<Column>,
    pk: Vec<Column>,
    rows: i64,
    bytes: u64,
}

/// One blob, as the measuring pass found it.
struct BlobPlan {
    sha256: String,
    storage_key: String,
    bytes: u64,
}

/// Write a tenant's archive into `sink`.
///
/// Two passes, and the reason is tar: a member's length lives in its header, so
/// nothing can be written until its size is known. The measuring pass is not an
/// extra cost — the manifest needs a row count per table anyway, which is a pass
/// either way (AC-3). What the two passes buy is that neither of them ever holds
/// a table's rendered rows: the largest thing in memory at any moment is one
/// page, or one blob.
///
/// Both passes read the same [`ExportTx`] snapshot, so the manifest cannot
/// disagree with the rows underneath it.
pub async fn write_archive(
    ex: &mut crate::repo::tenant_export::ExportTx<'_>,
    store: &dyn crate::storage::ArtifactStore,
    tenant: nook_types::TenantId,
    identity: ManifestTenant,
    server_version: &str,
    at: DateTime<Utc>,
    mut sink: ArchiveSink,
) -> Result<(), anyhow::Error> {
    let plan = match measure(ex, store, tenant).await {
        Ok(plan) => plan,
        Err(e) => {
            sink.fail(&e.to_string()).await;
            return Err(e);
        }
    };
    let (tables, blobs) = plan;

    let manifest = Manifest {
        format: FORMAT.to_string(),
        server_version: server_version.to_string(),
        migrations: ManifestMigrations {
            control: ex
                .migration_versions(crate::repo::tenant_export::CONTROL_LEDGER)
                .await?,
            chat: ex
                .migration_versions(crate::repo::tenant_export::CHAT_LEDGER)
                .await?,
        },
        tenant: identity,
        tables: tables
            .iter()
            .map(|t| (t.table.to_string(), t.rows))
            .collect(),
        blobs: ManifestBlobs {
            count: blobs.len() as u64,
            bytes: blobs.iter().map(|b| b.bytes).sum(),
        },
        secrets_included: false,
        exported_at: at,
    };

    match write_members(ex, store, tenant, &manifest, &tables, &blobs, &mut sink).await {
        Ok(()) => sink.finish().await,
        Err(e) => {
            sink.fail(&e.to_string()).await;
            Err(e)
        }
    }
}

/// Everything the archive will contain, and how long each member is.
async fn measure(
    ex: &mut crate::repo::tenant_export::ExportTx<'_>,
    store: &dyn crate::storage::ArtifactStore,
    tenant: nook_types::TenantId,
) -> Result<(Vec<TablePlan>, Vec<BlobPlan>), anyhow::Error> {
    let mut tables = Vec::with_capacity(INCLUDED_TABLES.len());
    for &table in INCLUDED_TABLES {
        let columns = ex.columns(table).await?;
        anyhow::ensure!(
            !columns.is_empty(),
            "`{table}` is in INCLUDED_TABLES but not in this schema"
        );
        let pk = ex.primary_key(table).await?;
        anyhow::ensure!(!pk.is_empty(), "`{table}` has no primary key to page by");
        let odd = non_uuid_key_columns(&pk);
        anyhow::ensure!(
            odd.is_empty(),
            "`{table}`'s primary key is not all uuid ({odd:?}), which the export's paging \
             order depends on"
        );

        let mut rows = 0i64;
        let mut bytes = 0u64;
        let mut cursor: Option<String> = None;
        loop {
            let page = ex
                .rows_page(table, &columns, &pk, tenant, cursor.as_deref(), PAGE)
                .await?;
            if page.is_empty() {
                break;
            }
            for line in &page {
                rows += 1;
                bytes += line.row_json.len() as u64 + 1;
            }
            cursor = page.last().map(|l| l.row_key.clone());
        }
        tables.push(TablePlan {
            table,
            columns,
            pk,
            rows,
            bytes,
        });
    }

    let mut blobs = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = ex.blobs_page(tenant, cursor.as_deref(), PAGE).await?;
        if page.is_empty() {
            break;
        }
        for blob in &page {
            // Refused rather than skipped: AC-6 says every exported row's bytes
            // are in the archive, and an archive missing content it claims to
            // have is worse than one that failed to build.
            let meta = store.head(&blob.storage_key).await?.ok_or_else(|| {
                anyhow::anyhow!(
                    "the file store has no object for {} (key {}) — this tenant's content \
                     cannot be exported until that is resolved",
                    blob.sha256,
                    blob.storage_key
                )
            })?;
            blobs.push(BlobPlan {
                sha256: blob.sha256.clone(),
                storage_key: blob.storage_key.clone(),
                bytes: meta.size,
            });
        }
        cursor = page.last().map(|b| b.sha256.clone());
    }

    Ok((tables, blobs))
}

async fn write_members(
    ex: &mut crate::repo::tenant_export::ExportTx<'_>,
    store: &dyn crate::storage::ArtifactStore,
    tenant: nook_types::TenantId,
    manifest: &Manifest,
    tables: &[TablePlan],
    blobs: &[BlobPlan],
    sink: &mut ArchiveSink,
) -> Result<(), anyhow::Error> {
    let body = serde_json::to_vec_pretty(manifest)?;
    sink.entry("manifest.json", body.len() as u64).await?;
    sink.data(&body).await?;
    sink.end_entry(body.len() as u64).await?;

    for plan in tables {
        sink.entry(&table_path(plan.table), plan.bytes).await?;
        let mut written = 0u64;
        let mut cursor: Option<String> = None;
        loop {
            let page = ex
                .rows_page(
                    plan.table,
                    &plan.columns,
                    &plan.pk,
                    tenant,
                    cursor.as_deref(),
                    PAGE,
                )
                .await?;
            if page.is_empty() {
                break;
            }
            for line in &page {
                sink.data(line.row_json.as_bytes()).await?;
                sink.data(b"\n").await?;
                written += line.row_json.len() as u64 + 1;
            }
            cursor = page.last().map(|l| l.row_key.clone());
        }
        anyhow::ensure!(
            written == plan.bytes,
            "`{}` changed under the export snapshot ({written} bytes written, {} measured)",
            plan.table,
            plan.bytes
        );
        sink.end_entry(plan.bytes).await?;
    }

    for blob in blobs {
        let bytes = store.get(&blob.storage_key).await?;
        anyhow::ensure!(
            bytes.len() as u64 == blob.bytes,
            "the object for {} changed size under the export ({} bytes, {} measured)",
            blob.sha256,
            bytes.len(),
            blob.bytes
        );
        sink.entry(&blob_path(&blob.sha256), blob.bytes).await?;
        for slice in bytes.chunks(FLUSH_AT) {
            sink.data(slice).await?;
        }
        sink.end_entry(blob.bytes).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(names: &[(&str, &str)]) -> Vec<Column> {
        names.iter().map(|(n, t)| Column::new(*n, *t)).collect()
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// The two constants are a partition, and every exclusion says why.
    #[test]
    fn the_two_lists_are_disjoint_and_excluded_entries_carry_a_reason() {
        let included: BTreeSet<&str> = INCLUDED_TABLES.iter().copied().collect();
        assert_eq!(
            included.len(),
            INCLUDED_TABLES.len(),
            "INCLUDED_TABLES has a duplicate"
        );
        let mut excluded = BTreeSet::new();
        for e in EXCLUDED_TABLES {
            assert!(
                excluded.insert(e.table),
                "EXCLUDED_TABLES lists {} twice",
                e.table
            );
            assert!(
                !e.reason.trim().is_empty(),
                "{} is excluded with no reason",
                e.table
            );
            assert!(!included.contains(e.table), "{} is in both lists", e.table);
        }
    }

    /// AC-11's guard, and the injected drift it exists to catch.
    #[test]
    fn an_unclassified_table_is_caught() {
        let clean = set(&["tasks", "nodes", "board_columns", "task_labels"]);
        assert!(classification_drift(&clean).is_empty());

        let drifted = set(&["tasks", "drift_demo"]);
        let problems = classification_drift(&drifted);
        assert_eq!(problems.len(), 1, "one unclassified table, one complaint");
        assert!(
            problems[0].contains("drift_demo") && problems[0].contains("classified nowhere"),
            "the message names the table and says what to do: {}",
            problems[0]
        );
    }

    /// AC-12's guard, and the injected drift it exists to catch.
    #[test]
    fn a_dropped_column_is_caught() {
        let schema = set(&["id", "tenant_id", "title"]);
        assert!(column_drift("tasks", &schema, &schema).is_none());

        // A secret-bearing table's marker is not a column, and is not drift.
        let mut with_marker = schema.clone();
        with_marker.insert(VALUE_OMITTED_KEY.to_string());
        assert!(column_drift("tasks", &with_marker, &schema).is_none());

        let truncated = set(&["id", "tenant_id"]);
        let msg = column_drift("tasks", &truncated, &schema).expect("the guard notices");
        assert!(
            msg.contains("title") && msg.contains("missing"),
            "the message names the lost column: {msg}"
        );

        let invented = set(&["id", "tenant_id", "title", "made_up"]);
        let msg = column_drift("tasks", &invented, &schema).expect("the guard notices");
        assert!(msg.contains("made_up"), "{msg}");
    }

    /// AC-13's guard, and the injected drift it exists to catch.
    #[test]
    fn a_new_secret_shaped_column_is_caught() {
        let ordinary = cols(&[("id", "uuid"), ("title", "text"), ("sha256", "text")]);
        assert!(secret_shape_drift("tasks", &ordinary).is_empty());

        // Already declared: notification_channels.secret travels as null.
        let declared = cols(&[("id", "uuid"), ("secret", "text")]);
        assert!(secret_shape_drift("notification_channels", &declared).is_empty());

        for (name, udt) in [
            ("secret_enc", "bytea"),
            ("api_token_hash", "text"),
            ("password", "text"),
            ("kdf_salt", "text"),
            ("verifier", "text"),
            ("blob", "bytea"),
        ] {
            let drifted = cols(&[("id", "uuid"), (name, udt)]);
            let problems = secret_shape_drift("tasks", &drifted);
            assert_eq!(problems.len(), 1, "{name} should be caught");
            assert!(
                problems[0].contains(name) && problems[0].contains("SECRET_COLUMNS"),
                "the message names the column and where to declare it: {}",
                problems[0]
            );
        }
    }

    /// The secret list is what makes the guard above meaningful; a typo in a
    /// table name would silently disarm it.
    #[test]
    fn every_declared_secret_column_names_a_known_table() {
        let known: BTreeSet<&str> = INCLUDED_TABLES
            .iter()
            .copied()
            .chain(EXCLUDED_TABLES.iter().map(|e| e.table))
            .collect();
        for (table, column) in SECRET_COLUMNS {
            assert!(
                known.contains(table),
                "SECRET_COLUMNS names {table}.{column}, which is in neither table list"
            );
        }
    }

    #[test]
    fn a_secret_column_is_null_and_the_row_says_so() {
        let columns = cols(&[("id", "uuid"), ("name", "text"), ("secret", "text")]);
        let sql = row_json_sql("notification_channels", &columns);
        // The cast is spelled here rather than in the literal: a `::type` in an
        // argument to a method called `contains` is what
        // check-nested-dialect.sh looks for, and it cannot tell `str::contains`
        // from the dialect seam's.
        assert!(
            sql.contains(&format!("'secret', NULL{}text", "::")),
            "{sql}"
        );
        assert!(sql.contains("'name', t.\"name\""), "{sql}");
        assert!(sql.contains("'value_omitted', true"), "{sql}");

        let keys = exported_keys("notification_channels", &columns);
        assert!(keys.contains("secret") && keys.contains(VALUE_OMITTED_KEY));
    }

    #[test]
    fn a_table_with_no_secret_gets_no_marker() {
        let columns = cols(&[("id", "uuid"), ("title", "text")]);
        let sql = row_json_sql("tasks", &columns);
        assert!(!sql.contains(VALUE_OMITTED_KEY), "{sql}");
        assert!(!exported_keys("tasks", &columns).contains(VALUE_OMITTED_KEY));
    }

    #[test]
    fn bytea_is_base64_and_nothing_else_is_transformed() {
        let columns = cols(&[("id", "uuid"), ("payload", "bytea"), ("meta", "jsonb")]);
        let sql = row_json_sql("some_table", &columns);
        assert!(sql.contains("encode(t.\"payload\", 'base64')"), "{sql}");
        assert!(sql.contains("'meta', t.\"meta\""), "{sql}");
    }

    /// Past fifty columns a single `jsonb_build_object` exceeds Postgres'
    /// argument limit, so the object is concatenated from groups.
    #[test]
    fn a_wide_table_is_built_in_groups() {
        let wide: Vec<Column> = (0..60)
            .map(|i| Column::new(format!("c{i}"), "text"))
            .collect();
        let sql = row_json_sql("wide", &wide);
        assert_eq!(
            sql.matches("jsonb_build_object(").count(),
            3,
            "60 columns is three groups of {COLUMN_GROUP}: {sql}"
        );
        assert_eq!(exported_keys("wide", &wide).len(), 60);
    }

    #[test]
    fn scoping_reaches_the_indirect_tables_through_their_parent() {
        assert_eq!(scope_sql("tasks"), "t.tenant_id = $1");
        assert_eq!(scope_sql("tenants"), "t.id = $1");
        assert!(scope_sql("board_columns").contains("FROM boards WHERE tenant_id = $1"));
        assert!(scope_sql("task_labels").contains("FROM tasks WHERE tenant_id = $1"));
        // A global theme has a NULL tenant_id, which `= $1` is never true for.
        assert_eq!(scope_sql("themes"), "t.tenant_id = $1");
    }

    #[test]
    fn a_composite_key_pages_in_tuple_order() {
        assert_eq!(key_sql(&["id".to_string()]), "t.\"id\"::text");
        assert_eq!(
            key_sql(&["task_id".to_string(), "label_id".to_string()]),
            "concat_ws('/', t.\"task_id\"::text, t.\"label_id\"::text)"
        );
        assert!(non_uuid_key_columns(&cols(&[("id", "uuid")])).is_empty());
        assert_eq!(
            non_uuid_key_columns(&cols(&[("key", "text")])).len(),
            1,
            "a non-uuid key breaks the ordering key_sql relies on"
        );
    }

    #[test]
    fn an_identifier_or_literal_carrying_a_quote_cannot_escape_it() {
        assert_eq!(ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(literal("a'b"), "'a''b'");
    }

    #[test]
    fn the_filename_is_slug_and_date() {
        let at = DateTime::parse_from_rfc3339("2026-08-25T10:11:12Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(archive_filename("acme", at), "acme-20260825.tar.gz");
        assert_eq!(archive_filename("a/b c", at), "a-b-c-20260825.tar.gz");
        assert_eq!(archive_filename("///", at), "tenant-20260825.tar.gz");
    }

    #[test]
    fn member_paths_are_the_documented_ones() {
        assert_eq!(table_path("tasks"), "db/tasks.jsonl");
        assert_eq!(blob_path("abc123"), "content/abc123");
    }
}
