//! MAIN-327: row mapping is engine-neutral.
//!
//! These run on WHICHEVER engine the bed is — `TestBed` picks from
//! `DATABASE_URL` — so `./test.sh rust` proves them on Postgres and
//! `./test.sh rust --sqlite` proves the same assertions on SQLite. That is the
//! whole point of the card: before it, a `text[]` DTO had a `SqliteRow` impl
//! that returned an error, so the second of those runs could not exist.

use nook_db::{params, Db, DbValue, FromDbRow};
use nook_testkit::TestBed;
use nook_types::*;
use uuid::Uuid;

/// A `text[]` column, which is the one column type the two engines do not
/// share: a real array on Postgres, JSON text on SQLite.
#[derive(Debug, FromDbRow)]
struct Channel {
    id: Uuid,
    levels: Vec<String>,
    kinds: Vec<String>,
    /// NULL in the fixture — proves the blanket `Option<T>` mapping, which
    /// resolves NULL by asking the ROW, not the type.
    secret: Option<String>,
    tenant_id: TenantId,
}

/// The three field attributes, mapped against the same row — plus a raw
/// identifier, whose column is `kind`, not `r#kind`. That last one is not
/// hypothetical: `TaskItem.r#type` exists, and getting it wrong is a runtime
/// `ColumnNotFound("r#type")`, not a compile error.
#[derive(Debug, FromDbRow)]
struct Attrs {
    #[db(rename = "kind")]
    renamed: String,
    r#kind: String,
    /// Not a column at all.
    #[db(skip)]
    filled_in: Option<String>,
    /// A column the SELECT below does not ask for.
    #[db(default)]
    not_selected: Option<String>,
}

async fn seed(bed: &TestBed, tenant: TenantId, levels: &[&str], kinds: &[&str]) -> Uuid {
    let id = Uuid::now_v7();
    bed.db()
        .exec(
            "INSERT INTO notification_channels (id, tenant_id, kind, name, config, levels, kinds)
             VALUES ($1, $2, 'webhook', 'n', '{}', $3, $4)",
            // `TextArray` is the non-optional text[] COLUMN arm (MAIN-310).
            // This used to be `Some(..)`, which reached the array binding only
            // as a side effect of `Option<Vec<String>>` converting differently
            // from `Vec<String>` — the accident that card removed. Naming the
            // position is what makes this a round-trip of the column bind.
            params![
                id,
                tenant,
                DbValue::TextArray(levels.iter().map(|s| s.to_string()).collect()),
                DbValue::TextArray(kinds.iter().map(|s| s.to_string()).collect())
            ],
        )
        .await
        .expect("seed channel");
    id
}

#[tokio::test]
async fn a_text_array_column_round_trips_on_this_engine() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("rowmap").await;
    let id = seed(&bed, tenant, &["warn", "error"], &["task.created"]).await;

    let got: Channel = bed
        .db()
        .query_one(
            "SELECT id, tenant_id, levels, kinds, secret FROM notification_channels WHERE id = $1",
            params![id],
        )
        .await
        .expect("read the channel back");

    assert_eq!(got.id, id);
    assert_eq!(got.levels, vec!["warn".to_string(), "error".to_string()]);
    assert_eq!(got.kinds, vec!["task.created".to_string()]);
    assert_eq!(
        got.tenant_id, tenant,
        "a domain newtype comes back through FromDbColumn"
    );
    assert!(got.secret.is_none(), "NULL maps to None, not to an error");

    bed.teardown().await;
}

#[tokio::test]
async fn an_empty_text_array_is_empty_and_not_an_error() {
    // The boring case that the old deferred SQLite impl also could not do, and
    // the one every unconfigured channel actually hits.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("rowmap").await;
    let id = seed(&bed, tenant, &[], &[]).await;

    let got: Channel = bed
        .db()
        .query_one(
            "SELECT id, tenant_id, levels, kinds, secret FROM notification_channels WHERE id = $1",
            params![id],
        )
        .await
        .expect("read");
    assert!(got.levels.is_empty() && got.kinds.is_empty());

    bed.teardown().await;
}

#[tokio::test]
async fn a_value_with_a_comma_is_one_element_not_two() {
    // Why the SQLite representation is JSON rather than a joined string. On
    // Postgres this has always held; the point is that it now holds on both.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("rowmap").await;
    let id = seed(&bed, tenant, &["a,b"], &[]).await;

    let got: Channel = bed
        .db()
        .query_one(
            "SELECT id, tenant_id, levels, kinds, secret FROM notification_channels WHERE id = $1",
            params![id],
        )
        .await
        .expect("read");
    assert_eq!(got.levels, vec!["a,b".to_string()]);

    bed.teardown().await;
}

#[tokio::test]
async fn the_field_attributes_do_what_their_sqlx_equivalents_did() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("rowmap").await;
    let id = seed(&bed, tenant, &[], &[]).await;

    // `not_selected` is deliberately absent from the SELECT list.
    let got: Attrs = bed
        .db()
        .query_one(
            "SELECT kind FROM notification_channels WHERE id = $1",
            params![id],
        )
        .await
        .expect("map with attributes");

    assert_eq!(got.renamed, "webhook", "#[db(rename)] reads another column");
    assert_eq!(
        got.r#kind, "webhook",
        "a raw identifier maps to `kind`, not `r#kind`"
    );
    assert!(got.filled_in.is_none(), "#[db(skip)] is not a column");
    assert!(
        got.not_selected.is_none(),
        "#[db(default)] tolerates a column the query did not select"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_column_the_row_does_not_have_is_an_error_naming_it() {
    // The counterpart to `#[db(default)]`: WITHOUT that attribute a missing
    // column must fail loudly. Mapping is by name, so a typo'd column is a
    // runtime failure — it had better be a legible one.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("rowmap").await;
    let id = seed(&bed, tenant, &[], &[]).await;

    let err = bed
        .db()
        .query_one::<Channel>(
            "SELECT id, tenant_id, levels, kinds FROM notification_channels WHERE id = $1",
            params![id],
        )
        .await
        .expect_err("secret was not selected and is not #[db(default)]");
    assert!(
        err.to_string().contains("secret"),
        "the error should name the column: {err}"
    );

    bed.teardown().await;
}

#[tokio::test]
async fn a_tuple_row_still_maps_by_position() {
    // `SELECT count(*)` has no column name to map by, which is why tuples keep
    // a positional impl.
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let tenant = bed.tenant("rowmap").await;
    seed(&bed, tenant, &[], &[]).await;

    let (n, kind): (i64, String) = bed
        .db()
        .query_one(
            "SELECT count(*), max(kind) FROM notification_channels WHERE tenant_id = $1",
            params![tenant],
        )
        .await
        .expect("tuple row");
    assert_eq!(n, 1);
    assert_eq!(kind, "webhook");

    bed.teardown().await;
}
