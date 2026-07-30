//! Boot-time collapse of a pre-squash migration ledger (MAIN-235).
//!
//! When a migration set is squashed into a single canonical `0001`
//! (`scripts/squash-migrations.sh`), every database that already applied the
//! old set has a ledger listing versions the new tree no longer contains. The
//! strict migrator sees those as "previously applied but missing in the
//! resolved migrations" and aborts the boot.
//!
//! The repo's one previous squash handled that by hand, and produced the near
//! miss recorded in CLAUDE.md: production's ledger was re-stamped to a single
//! row while it was still running an image that embedded all nineteen
//! migrations, so its next restart would have re-applied `0002`..`0019` against
//! a schema that already had them. The lesson is not "be careful with the
//! re-stamp"; it is that **the re-stamp must not be a separate step at all**.
//!
//! So it lives here, and the image that ships the squash performs it itself, in
//! one transaction, immediately before the migrator runs. There is no ordering
//! left for an operator to get wrong.
//!
//! The safety property is the manifest. It names the exact ledger the collapse
//! is valid for — every old version AND its checksum — and the collapse fires
//! only on an exact match. Four outcomes, no fifth:
//!
//! | ledger                                   | outcome                        |
//! |------------------------------------------|--------------------------------|
//! | empty (virgin database)                  | nothing to do; migrator applies |
//! | already carries the new row              | no-op, however many migrations followed it |
//! | exactly the manifest's old set           | collapsed, in one transaction  |
//! | anything else                            | untouched, loud [`RestampError::LedgerMismatch`] |
//!
//! That last row is deliberately a refusal and not a repair. A ledger we do not
//! recognise is one we cannot reason about, and rewriting it on a guess is how
//! a schema silently stops matching what the repo says it is. In production the
//! refusal stops the boot (the caller propagates it); in dev the caller logs it
//! and lets MAIN-224's tolerance carry the boot, because a developer's stray
//! branch row is the overwhelmingly likely cause there.

use sqlx::{PgPool, Row};

/// A parsed `squash-manifest.txt`: the single row a recognised pre-squash
/// ledger collapses to, and the exact ledger that qualifies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SquashManifest {
    pub new_version: i64,
    pub new_description: String,
    pub new_checksum: Vec<u8>,
    /// `(version, checksum)` for every migration the squash replaced, in file
    /// order. Both halves matter: versions alone would let a ledger with the
    /// right numbers but different content pass.
    pub old: Vec<(i64, Vec<u8>)>,
}

/// What the re-stamp did. Every variant is a normal, expected boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restamp {
    /// No manifest is embedded — this build ships no squash.
    NoManifest,
    /// A virgin database. The migrator will apply the canonical `0001` itself.
    EmptyLedger,
    /// The new row is already present; nothing to collapse. Also the steady
    /// state for every boot after the first, and for a database that has since
    /// applied post-squash migrations.
    AlreadySquashed,
    /// A recognised pre-squash ledger was collapsed to the single new row.
    Collapsed { replaced: usize },
}

#[derive(Debug, thiserror::Error)]
pub enum RestampError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    /// The ledger is neither virgin, nor already squashed, nor exactly the set
    /// the manifest describes. Left completely untouched.
    #[error(
        "this database's migration ledger does not match the squash manifest, so it \
         was left untouched: {detail}. The squash can only collapse a ledger that is \
         exactly the set it replaced. Heal it deliberately (scripts/dev-db-heal.sh in \
         dev) or restore from the pre-squash image — do not hand-edit the ledger."
    )]
    LedgerMismatch { detail: String },
}

/// Parse a manifest. `None` when the text carries no `new` line — an absent or
/// commented-out manifest means "this build ships no squash", never an error,
/// so `include_str!` of a placeholder file is safe.
///
/// Format (generated; see `scripts/squash-migrations.sh`):
/// ```text
/// # comments and blank lines ignored
/// set control
/// new 1 <sha384-hex> init
/// old 1 <sha384-hex>
/// old 2 <sha384-hex>
/// ```
pub fn parse_manifest(text: &str) -> Option<SquashManifest> {
    let mut new: Option<(i64, String, Vec<u8>)> = None;
    let mut old = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("new") => {
                let version = parts.next()?.parse().ok()?;
                let checksum = hex_bytes(parts.next()?)?;
                // The description is the rest of the line; sqlx stores it but
                // never validates it, so a missing one is not fatal.
                let description = parts.collect::<Vec<_>>().join(" ");
                new = Some((version, description, checksum));
            }
            Some("old") => {
                let version = parts.next()?.parse().ok()?;
                old.push((version, hex_bytes(parts.next()?)?));
            }
            // `set <name>` and anything unrecognised are metadata for humans.
            _ => {}
        }
    }

    let (new_version, new_description, new_checksum) = new?;
    Some(SquashManifest {
        new_version,
        new_description,
        new_checksum,
        old,
    })
}

fn hex_bytes(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Collapse a recognised pre-squash ledger, or leave it exactly as it is.
///
/// `table` is the ledger's qualified name — `_sqlx_migrations` resolves through
/// the connection's `search_path`, which is what keeps nook-chat's ledger in its
/// own schema separate from the control plane's.
///
/// Runs before the migrator. Idempotent: safe on every boot forever.
pub async fn restamp(
    pool: &PgPool,
    manifest: Option<&SquashManifest>,
    table: &str,
) -> Result<Restamp, RestampError> {
    let Some(manifest) = manifest else {
        return Ok(Restamp::NoManifest);
    };

    // No ledger table at all is a virgin database: the migrator creates it.
    let exists: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
        .bind(table)
        .fetch_one(pool)
        .await?;
    if exists.is_none() {
        return Ok(Restamp::EmptyLedger);
    }

    let rows = sqlx::query(&format!(
        "SELECT version, checksum FROM {table} ORDER BY version"
    ))
    .fetch_all(pool)
    .await?;
    let ledger: Vec<(i64, Vec<u8>)> = rows
        .iter()
        .map(|r| (r.get::<i64, _>("version"), r.get::<Vec<u8>, _>("checksum")))
        .collect();

    if ledger.is_empty() {
        return Ok(Restamp::EmptyLedger);
    }

    // Already squashed? The new row being present with the RIGHT checksum is
    // the whole test — deliberately not "the ledger is exactly one row", so a
    // database that has since applied 0002, 0003… is still recognised. Getting
    // this wrong would turn every post-squash migration into a false mismatch.
    if ledger
        .iter()
        .any(|(v, c)| *v == manifest.new_version && *c == manifest.new_checksum)
    {
        return Ok(Restamp::AlreadySquashed);
    }

    if ledger != manifest.old {
        return Err(RestampError::LedgerMismatch {
            detail: describe_mismatch(&ledger, &manifest.old),
        });
    }

    // One transaction: the old rows and the new row are never both absent, and
    // never both present, from any other connection's point of view. A crash
    // mid-collapse rolls back to the pre-squash ledger and the next boot
    // retries — which is exactly why this is safe to do unattended.
    let mut tx = pool.begin().await?;
    sqlx::query(&format!("DELETE FROM {table}"))
        .execute(&mut *tx)
        .await?;
    sqlx::query(&format!(
        "INSERT INTO {table} (version, description, installed_on, success, checksum, execution_time)
         VALUES ($1, $2, now(), true, $3, 0)"
    ))
    .bind(manifest.new_version)
    .bind(&manifest.new_description)
    .bind(&manifest.new_checksum)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(Restamp::Collapsed {
        replaced: manifest.old.len(),
    })
}

/// Say precisely how the ledger differs, because the operator reading this is
/// deciding whether their database is safe. "Does not match" alone would send
/// them to hand-edit the ledger, which is the failure this whole module exists
/// to prevent.
fn describe_mismatch(ledger: &[(i64, Vec<u8>)], expected: &[(i64, Vec<u8>)]) -> String {
    let have: Vec<i64> = ledger.iter().map(|(v, _)| *v).collect();
    let want: Vec<i64> = expected.iter().map(|(v, _)| *v).collect();

    if have != want {
        let extra: Vec<i64> = have.iter().copied().filter(|v| !want.contains(v)).collect();
        let missing: Vec<i64> = want.iter().copied().filter(|v| !have.contains(v)).collect();
        return format!(
            "the applied versions differ from the squashed set (applied {} version(s), \
             manifest describes {}){}{}",
            have.len(),
            want.len(),
            if extra.is_empty() {
                String::new()
            } else {
                format!("; applied but not in the manifest: {extra:?}")
            },
            if missing.is_empty() {
                String::new()
            } else {
                format!("; in the manifest but not applied: {missing:?}")
            },
        );
    }

    // Same versions, different content: someone edited an applied migration.
    let differing: Vec<i64> = ledger
        .iter()
        .zip(expected)
        .filter(|((_, a), (_, b))| a != b)
        .map(|((v, _), _)| *v)
        .collect();
    format!(
        "the applied versions match but their checksums do not, for version(s) \
         {differing:?} — an applied migration's content changed, which the squash \
         cannot account for"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# a comment
set control
new 1 aabb init schema
old 1 1122
old 2 33ff
";

    #[test]
    fn parses_the_generated_format() {
        let m = parse_manifest(SAMPLE).expect("parses");
        assert_eq!(m.new_version, 1);
        assert_eq!(m.new_description, "init schema");
        assert_eq!(m.new_checksum, vec![0xaa, 0xbb]);
        assert_eq!(m.old, vec![(1, vec![0x11, 0x22]), (2, vec![0x33, 0xff])]);
    }

    #[test]
    fn a_manifest_with_no_new_line_is_absent_not_broken() {
        // The placeholder a build ships when it carries no squash.
        assert!(parse_manifest("# nothing here\nset control\n").is_none());
        assert!(parse_manifest("").is_none());
    }

    #[test]
    fn rejects_malformed_hex_rather_than_guessing() {
        assert!(parse_manifest("new 1 xyz init\n").is_none());
        assert!(parse_manifest("new 1 abc init\n").is_none()); // odd length
    }

    #[test]
    fn mismatch_messages_name_what_is_wrong() {
        let extra = describe_mismatch(&[(1, vec![1]), (2, vec![2])], &[(1, vec![1])]);
        assert!(
            extra.contains("applied but not in the manifest: [2]"),
            "{extra}"
        );

        let missing = describe_mismatch(&[(1, vec![1])], &[(1, vec![1]), (2, vec![2])]);
        assert!(
            missing.contains("in the manifest but not applied: [2]"),
            "{missing}"
        );

        let edited = describe_mismatch(&[(1, vec![9])], &[(1, vec![1])]);
        assert!(edited.contains("checksums do not"), "{edited}");
        assert!(edited.contains("[1]"), "{edited}");
    }
}
