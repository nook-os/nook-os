//! Reading a tenant out, one consistent snapshot at a time (MAIN-659).
//!
//! Every other repository here owns an aggregate. This one owns a *pass over
//! the whole schema*, which is why its queries are generated rather than
//! written: the export must carry whatever columns a table has today, so a
//! hand-written SELECT list is precisely the thing it must not contain (AC-12).
//! The generators are pure functions in
//! [`crate::services::tenant_archive`]; this module is where they meet a
//! connection.
//!
//! **One read-only REPEATABLE READ transaction covers the entire export.** The
//! manifest carries a row count per table and tar puts each member's length in
//! its header, so the export reads every table twice — once to measure, once to
//! write. Under READ COMMITTED those two passes would see different databases
//! and the archive would disagree with its own manifest.
//!
//! **Postgres only.** Column and key discovery is `information_schema` /
//! `pg_index`; the route refuses any other engine before reaching here.

use std::collections::BTreeSet;

use nook_db::{params, DbPool, DbTx};
use nook_types::TenantId;

use crate::error::{ApiError, ApiResult};
use crate::services::tenant_archive::{self, BlobRef, Column, ManifestTenant, RowLine};

/// The ledger a migrator set records into.
pub const CONTROL_LEDGER: &str = "public._sqlx_migrations";
pub const CHAT_LEDGER: &str = "chat._sqlx_migrations";

#[derive(nook_db::FromDbRow)]
struct NameRow {
    name: String,
}

#[derive(nook_db::FromDbRow)]
struct ColumnRow {
    column_name: String,
    udt_name: String,
}

#[derive(nook_db::FromDbRow)]
struct VersionRow {
    version: i64,
}

#[derive(nook_db::FromDbRow)]
struct TenantRow {
    id: uuid::Uuid,
    name: String,
    slug: String,
}

#[derive(nook_db::FromDbRow)]
struct ExistsRow {
    present: bool,
}

/// The snapshot an export reads through.
pub struct ExportTx<'c> {
    tx: DbTx<'c>,
}

/// Open the snapshot.
///
/// `READ ONLY` is a statement of intent the database can enforce: nothing about
/// an export may write, and a bug that tried would fail here rather than in
/// somebody's data. `TimeZone` is pinned so a `timestamptz` renders with a
/// `+00:00` offset regardless of what the server's default happens to be —
/// otherwise the same row exports differently on two deployments.
pub async fn begin(db: &DbPool) -> ApiResult<ExportTx<'_>> {
    let mut tx = db.begin().await.map_err(nook_db::DbError::Query)?;
    tx.exec(
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY",
        params![],
    )
    .await?;
    tx.exec("SET LOCAL TimeZone = 'UTC'", params![]).await?;
    Ok(ExportTx { tx })
}

impl ExportTx<'_> {
    /// The tenant, or `None` if there is no such row.
    pub async fn tenant(&mut self, tenant: TenantId) -> ApiResult<Option<ManifestTenant>> {
        let row: Option<TenantRow> = self
            .tx
            .query_opt(
                "SELECT id, name, slug FROM tenants WHERE id = $1",
                params![tenant],
            )
            .await?;
        Ok(row.map(|r| ManifestTenant {
            id: r.id,
            name: r.name,
            slug: r.slug,
        }))
    }

    /// Every table the live schema scopes to a tenant, plus the ones that reach
    /// a tenant through a parent — the universe AC-11's guard demands be
    /// classified.
    pub async fn tenant_scoped_tables(&mut self) -> ApiResult<BTreeSet<String>> {
        let rows: Vec<NameRow> = self
            .tx
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
            .await?;
        let mut names: BTreeSet<String> = rows.into_iter().map(|r| r.name).collect();
        for extra in tenant_archive::INDIRECTLY_SCOPED_TABLES {
            if self.table_exists(extra).await? {
                names.insert((*extra).to_string());
            }
        }
        Ok(names)
    }

    /// `table`'s columns, in declaration order.
    pub async fn columns(&mut self, table: &str) -> ApiResult<Vec<Column>> {
        let rows: Vec<ColumnRow> = self
            .tx
            .query_all(
                "SELECT column_name, udt_name
                   FROM information_schema.columns
                  WHERE table_schema = 'public' AND table_name = $1
                  ORDER BY ordinal_position",
                params![table.to_string()],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| Column::new(r.column_name, r.udt_name))
            .collect())
    }

    /// `table`'s primary-key columns, in key order.
    pub async fn primary_key(&mut self, table: &str) -> ApiResult<Vec<Column>> {
        let rows: Vec<ColumnRow> = self
            .tx
            .query_all(
                "SELECT a.attname AS column_name, ty.typname AS udt_name
                   FROM pg_class c
                   JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'public'
                   JOIN pg_index i ON i.indrelid = c.oid AND i.indisprimary
                   JOIN LATERAL unnest(i.indkey) WITH ORDINALITY AS k(attnum, ord) ON true
                   JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = k.attnum
                   JOIN pg_type ty ON ty.oid = a.atttypid
                  WHERE c.relname = $1
                  ORDER BY k.ord",
                params![table.to_string()],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| Column::new(r.column_name, r.udt_name))
            .collect())
    }

    /// The applied versions of one migrator set, or nothing when that set has
    /// never run here — a control plane with no chat schema is an ordinary
    /// deployment, not a failure.
    pub async fn migration_versions(&mut self, ledger: &str) -> ApiResult<Vec<i64>> {
        if !self.relation_exists(ledger).await? {
            return Ok(Vec::new());
        }
        let rows: Vec<VersionRow> = self
            .tx
            .query_all(
                &format!("SELECT version FROM {ledger} ORDER BY version"),
                params![],
            )
            .await?;
        Ok(rows.into_iter().map(|r| r.version).collect())
    }

    /// One page of `table`'s rows, rendered as JSON, ordered by primary key.
    ///
    /// `after` is the previous page's last key; `None` starts at the beginning.
    /// Keyset rather than `OFFSET` so a wide table costs one index walk instead
    /// of re-scanning what has already been written.
    pub async fn rows_page(
        &mut self,
        table: &str,
        columns: &[Column],
        pk: &[Column],
        tenant: TenantId,
        after: Option<&str>,
        limit: i64,
    ) -> ApiResult<Vec<RowLine>> {
        let names: Vec<String> = pk.iter().map(|c| c.name.clone()).collect();
        let key = tenant_archive::key_sql(&names);
        let scope = tenant_archive::scope_sql(table);
        let json = tenant_archive::row_json_sql(table, columns);

        match after {
            None => {
                let sql = format!(
                    "SELECT ({json})::text AS row_json, {key} AS row_key
                       FROM {table} t
                      WHERE {scope}
                      ORDER BY row_key
                      LIMIT {limit}"
                );
                Ok(self.tx.query_all(&sql, params![tenant]).await?)
            }
            Some(cursor) => {
                let sql = format!(
                    "SELECT ({json})::text AS row_json, {key} AS row_key
                       FROM {table} t
                      WHERE {scope} AND {key} > $2
                      ORDER BY row_key
                      LIMIT {limit}"
                );
                Ok(self
                    .tx
                    .query_all(&sql, params![tenant, cursor.to_string()])
                    .await?)
            }
        }
    }

    /// One page of the tenant's DISTINCT blobs, ordered by digest.
    ///
    /// Distinct by `sha256`, because the archive stores content by digest: two
    /// rows that uploaded the same bytes are one entry (AC-6).
    pub async fn blobs_page(
        &mut self,
        tenant: TenantId,
        after: Option<&str>,
        limit: i64,
    ) -> ApiResult<Vec<BlobRef>> {
        match after {
            None => {
                let sql = format!(
                    "SELECT DISTINCT ON (sha256) sha256, storage_key
                       FROM user_content
                      WHERE tenant_id = $1
                      ORDER BY sha256, id
                      LIMIT {limit}"
                );
                Ok(self.tx.query_all(&sql, params![tenant]).await?)
            }
            Some(cursor) => {
                let sql = format!(
                    "SELECT DISTINCT ON (sha256) sha256, storage_key
                       FROM user_content
                      WHERE tenant_id = $1 AND sha256 > $2
                      ORDER BY sha256, id
                      LIMIT {limit}"
                );
                Ok(self
                    .tx
                    .query_all(&sql, params![tenant, cursor.to_string()])
                    .await?)
            }
        }
    }

    async fn table_exists(&mut self, table: &str) -> ApiResult<bool> {
        self.relation_exists(&format!("public.{table}")).await
    }

    /// Asked before every query against a relation that may not be there.
    ///
    /// Not "run it and catch the error": in Postgres a failed statement aborts
    /// the whole transaction, so a missing chat ledger would take the export
    /// down with it.
    async fn relation_exists(&mut self, qualified: &str) -> ApiResult<bool> {
        let row: ExistsRow = self
            .tx
            .query_one(
                "SELECT to_regclass($1) IS NOT NULL AS present",
                params![qualified.to_string()],
            )
            .await?;
        Ok(row.present)
    }

    /// Let the snapshot go. An export writes nothing, so this is a release
    /// rather than a commit; failing to end it cleanly is not the caller's
    /// problem.
    pub async fn finish(self) {
        let _ = self.tx.rollback().await;
    }
}

/// The engine gate, stated once.
///
/// Column discovery, the row encoding and base64 for `bytea` are all Postgres.
/// A SQLite control plane gets a refusal that says so rather than an archive
/// that is quietly a different shape.
pub fn require_postgres(db: &DbPool) -> ApiResult<()> {
    if db.engine() == nook_db::Engine::Postgres {
        return Ok(());
    }
    Err(ApiError::ServiceUnavailable(
        "tenant export needs a Postgres control plane — the archive's row encoding and \
         column discovery are Postgres-only"
            .into(),
    ))
}
