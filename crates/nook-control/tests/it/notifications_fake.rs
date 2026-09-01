//! Notification and feedback callers against the in-memory fakes, with **no
//! database at all** (MAIN-256 AC-3).
//!
//! The rules worth pinning here are the ones whose failure is silent: an inbox
//! scope that showed somebody else's notification, a channel read that handed
//! a signing secret to the settings page, an edit that blanked a token the UI
//! never saw, and a delivery failure that erased the record of the last
//! success.
//!
//! `cargo test -p nook-control --test notifications_fake` passes with the
//! database stopped.

use nook_control::repo::notifications::{
    ChannelEdit, FakeFeedbackRepository, FakeNotificationRepository, FeedbackRepository,
    FeedbackSetting, NewChannel, NewNotification, NotificationFilter, NotificationRepository,
};
use nook_types::*;
use uuid::Uuid;

fn tenant() -> TenantId {
    TenantId::new()
}

async fn raise(
    repo: &FakeNotificationRepository,
    t: TenantId,
    to: Option<UserId>,
    title: &str,
) -> Uuid {
    repo.raise(NewNotification {
        tenant: t,
        user_id: to.map(|u| u.0),
        level: "info".into(),
        title: title.into(),
        body: String::new(),
        kind: "test".into(),
        link: None,
        payload: serde_json::json!({}),
    })
    .await
    .unwrap()
    .id
}

fn all(limit: i64) -> NotificationFilter {
    NotificationFilter {
        unread_only: false,
        limit,
    }
}

// ── the inbox scope ─────────────────────────────────────────────────────────

#[tokio::test]
async fn the_inbox_shows_mine_and_everyones_but_never_somebody_elses() {
    let repo = FakeNotificationRepository::new();
    let t = tenant();
    let (alice, bob) = (UserId::new(), UserId::new());

    raise(&repo, t, Some(alice), "for alice").await;
    raise(&repo, t, Some(bob), "for bob").await;
    raise(&repo, t, None, "for everyone").await;

    let mut titles: Vec<String> = repo
        .list(t, alice, all(50))
        .await
        .unwrap()
        .into_iter()
        .map(|n| n.title)
        .collect();
    titles.sort();
    assert_eq!(
        titles,
        vec!["for alice", "for everyone"],
        "a tenant-wide row reaches everyone; a targeted one reaches only its \
         person"
    );
    assert_eq!(repo.unread_count(t, alice).await.unwrap(), 2);
}

#[tokio::test]
async fn another_tenants_notifications_are_invisible() {
    let repo = FakeNotificationRepository::new();
    let (mine, theirs) = (tenant(), tenant());
    let me = UserId::new();
    raise(&repo, theirs, None, "theirs").await;

    assert!(repo.list(mine, me, all(50)).await.unwrap().is_empty());
    assert_eq!(repo.unread_count(mine, me).await.unwrap(), 0);
    assert_eq!(repo.clear(mine, me).await.unwrap(), 0);
    assert_eq!(repo.inbox_len(), 1, "a wrong-tenant clear removes nothing");
}

#[tokio::test]
async fn re_reading_does_not_move_the_read_timestamp() {
    let repo = FakeNotificationRepository::new();
    let t = tenant();
    let me = UserId::new();
    let id = raise(&repo, t, Some(me), "n").await;

    assert_eq!(repo.mark_read(id, t).await.unwrap(), 1);
    assert_eq!(
        repo.mark_read(id, t).await.unwrap(),
        0,
        "`AND read_at IS NULL` — a second read matches nothing rather than \
         re-stamping when it was first seen"
    );
    assert_eq!(repo.is_read(id), Some(true));
}

#[tokio::test]
async fn mark_all_read_covers_tenant_wide_rows_too_and_stops_at_the_tenant() {
    let repo = FakeNotificationRepository::new();
    let (t, other) = (tenant(), tenant());
    let (me, someone) = (UserId::new(), UserId::new());
    let mine = raise(&repo, t, Some(me), "mine").await;
    let shared = raise(&repo, t, None, "shared").await;
    let theirs = raise(&repo, t, Some(someone), "theirs").await;
    let elsewhere = raise(&repo, other, None, "elsewhere").await;

    assert_eq!(repo.mark_all_read(t, me).await.unwrap(), 2);
    assert_eq!(repo.is_read(mine), Some(true));
    assert_eq!(repo.is_read(shared), Some(true));
    assert_eq!(
        repo.is_read(theirs),
        Some(false),
        "marking mine read must not read somebody else's"
    );
    assert_eq!(repo.is_read(elsewhere), Some(false));
}

#[tokio::test]
async fn the_unread_filter_and_limit_both_apply() {
    let repo = FakeNotificationRepository::new();
    let t = tenant();
    let me = UserId::new();
    let read = raise(&repo, t, Some(me), "read").await;
    raise(&repo, t, Some(me), "unread-a").await;
    raise(&repo, t, Some(me), "unread-b").await;
    repo.mark_read(read, t).await.unwrap();

    let unread = repo
        .list(
            t,
            me,
            NotificationFilter {
                unread_only: true,
                limit: 50,
            },
        )
        .await
        .unwrap();
    assert_eq!(unread.len(), 2);
    assert!(unread.iter().all(|n| n.read_at.is_none()));
    assert_eq!(repo.list(t, me, all(1)).await.unwrap().len(), 1);
}

// ── channels: the secret has exactly two readers ────────────────────────────

#[tokio::test]
async fn the_settings_page_read_carries_no_secret_and_no_config() {
    let repo = FakeNotificationRepository::new();
    let t = tenant();
    let created = repo
        .create_channel(NewChannel {
            tenant: t,
            kind: "ntfy".into(),
            name: "alerts".into(),
            config: serde_json::json!({ "topic": "nook", "token": "in-config" }),
            levels: vec![],
            kinds: vec![],
            secret: Some("signing-secret".into()),
        })
        .await
        .unwrap();

    // `NotificationChannel` has no field that could carry either — the type is
    // the guarantee. Asserting the send paths DO get them keeps that meaningful.
    let listed = repo.list_channels(t).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "alerts");

    let target = repo
        .channel_target(created.id, t)
        .await
        .unwrap()
        .expect("the test-send path gets one");
    assert_eq!(target.secret.as_deref(), Some("signing-secret"));
    assert_eq!(target.config["topic"], "nook");
}

#[tokio::test]
async fn only_enabled_channels_are_fanned_out_to() {
    let repo = FakeNotificationRepository::new();
    let t = tenant();
    let mk = |name: &str| NewChannel {
        tenant: t,
        kind: "ntfy".into(),
        name: name.into(),
        config: serde_json::json!({}),
        levels: vec![],
        kinds: vec![],
        secret: None,
    };
    let on = repo.create_channel(mk("on")).await.unwrap();
    let off = repo.create_channel(mk("off")).await.unwrap();
    repo.update_channel(
        off.id,
        t,
        ChannelEdit {
            enabled: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let targets = repo.enabled_channels(t).await.unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].id, on.id);
}

#[tokio::test]
async fn renaming_a_channel_keeps_the_config_the_ui_never_saw() {
    let repo = FakeNotificationRepository::new();
    let t = tenant();
    let c = repo
        .create_channel(NewChannel {
            tenant: t,
            kind: "ntfy".into(),
            name: "before".into(),
            config: serde_json::json!({ "token": "secret-in-config" }),
            levels: vec![],
            kinds: vec![],
            secret: Some("sig".into()),
        })
        .await
        .unwrap();

    repo.update_channel(
        c.id,
        t,
        ChannelEdit {
            name: Some("after".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(
        repo.config_of(c.id).unwrap()["token"],
        "secret-in-config",
        "COALESCE on config: a UI that cannot read the token back must be able \
         to rename without blanking it"
    );
    assert_eq!(repo.secret_of(c.id), Some(Some("sig".into())));
}

#[tokio::test]
async fn a_delivery_failure_does_not_erase_the_last_success() {
    let repo = FakeNotificationRepository::new();
    let t = tenant();
    let c = repo
        .create_channel(NewChannel {
            tenant: t,
            kind: "ntfy".into(),
            name: "c".into(),
            config: serde_json::json!({}),
            levels: vec![],
            kinds: vec![],
            secret: None,
        })
        .await
        .unwrap();

    repo.record_outcome(c.id, true, None).await.unwrap();
    let after_ok = repo.list_channels(t).await.unwrap()[0].clone();
    assert!(after_ok.last_ok_at.is_some());
    assert_eq!(after_ok.last_error, None);

    repo.record_outcome(c.id, false, Some("connection refused"))
        .await
        .unwrap();
    let after_fail = repo.list_channels(t).await.unwrap()[0].clone();
    assert_eq!(
        after_fail.last_ok_at, after_ok.last_ok_at,
        "the CASE only writes last_ok_at on success — 'worked at 09:00, \
         failing since' is the shape that tells you something"
    );
    assert_eq!(after_fail.last_error.as_deref(), Some("connection refused"));
}

#[tokio::test]
async fn a_channel_belongs_to_its_tenant() {
    let repo = FakeNotificationRepository::new();
    let (mine, theirs) = (tenant(), tenant());
    let c = repo
        .create_channel(NewChannel {
            tenant: theirs,
            kind: "ntfy".into(),
            name: "theirs".into(),
            config: serde_json::json!({}),
            levels: vec![],
            kinds: vec![],
            secret: Some("sig".into()),
        })
        .await
        .unwrap();

    assert!(repo.list_channels(mine).await.unwrap().is_empty());
    assert!(repo.channel_target(c.id, mine).await.unwrap().is_none());
    assert_eq!(repo.delete_channel(c.id, mine).await.unwrap(), 0);
    assert!(repo
        .update_channel(
            c.id,
            mine,
            ChannelEdit {
                name: Some("hijacked".into()),
                ..Default::default()
            }
        )
        .await
        .unwrap()
        .is_none());
}

// ── feedback ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_personal_setting_overrides_the_tenant_wide_one() {
    let repo = FakeFeedbackRepository::new();
    let t = tenant();
    let (me, other) = (UserId::new(), UserId::new());

    repo.set_tenant_setting(
        t,
        FeedbackSetting::Workspace,
        serde_json::json!("shared-workspace"),
    );
    assert_eq!(
        repo.setting(t, me, FeedbackSetting::Workspace)
            .await
            .unwrap(),
        Some("shared-workspace".into()),
        "with no personal row, the tenant-wide one is the fallback"
    );

    repo.set_setting(t, me, FeedbackSetting::Workspace, serde_json::json!("mine"))
        .await
        .unwrap();
    assert_eq!(
        repo.setting(t, me, FeedbackSetting::Workspace)
            .await
            .unwrap(),
        Some("mine".into()),
        "`ORDER BY (user_id = $3) DESC` — my own row wins"
    );
    assert_eq!(
        repo.setting(t, other, FeedbackSetting::Workspace)
            .await
            .unwrap(),
        Some("shared-workspace".into()),
        "…and does not become everybody's"
    );
}

#[tokio::test]
async fn the_three_feedback_settings_are_independent() {
    let repo = FakeFeedbackRepository::new();
    let t = tenant();
    let me = UserId::new();

    repo.set_setting(t, me, FeedbackSetting::Branch, serde_json::json!("main"))
        .await
        .unwrap();
    assert_eq!(
        repo.setting(t, me, FeedbackSetting::Branch).await.unwrap(),
        Some("main".into())
    );
    assert_eq!(
        repo.setting(t, me, FeedbackSetting::Workspace)
            .await
            .unwrap(),
        None,
        "setting one key must not answer for another"
    );
    assert_eq!(
        repo.setting(t, me, FeedbackSetting::Instructions)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn setting_the_same_key_twice_replaces_rather_than_stacks() {
    let repo = FakeFeedbackRepository::new();
    let t = tenant();
    let me = UserId::new();

    repo.set_setting(t, me, FeedbackSetting::Branch, serde_json::json!("one"))
        .await
        .unwrap();
    repo.set_setting(t, me, FeedbackSetting::Branch, serde_json::json!("two"))
        .await
        .unwrap();
    assert_eq!(
        repo.setting(t, me, FeedbackSetting::Branch).await.unwrap(),
        Some("two".into()),
        "ON CONFLICT … DO UPDATE, not a second row the read might pick from"
    );
}

#[tokio::test]
async fn feedback_is_recorded_queued_and_then_says_what_actually_happened() {
    let repo = FakeFeedbackRepository::new();
    let t = tenant();
    let me = UserId::new();

    let item = repo
        .submit(t, Some(WorkspaceId::new()), None, "it crashed", me)
        .await
        .unwrap();
    assert_eq!(item.status, "queued");

    // 'queued' is not a holding pattern — nothing retries it, so the terminal
    // status has to be written.
    repo.set_status(item.id, "delivered").await.unwrap();
    assert_eq!(repo.status_of(item.id).as_deref(), Some("delivered"));

    let updated = repo
        .update(item.id, t, None, Some("https://example/pr/1".into()))
        .await
        .unwrap()
        .expect("recorded");
    assert_eq!(updated.pr_url.as_deref(), Some("https://example/pr/1"));
    assert_eq!(
        updated.status, "delivered",
        "a COALESCE'd None must not reset the status"
    );
}

#[tokio::test]
async fn feedback_is_scoped_to_its_tenant() {
    let repo = FakeFeedbackRepository::new();
    let (mine, theirs) = (tenant(), tenant());
    let item = repo
        .submit(theirs, None, None, "theirs", UserId::new())
        .await
        .unwrap();

    assert!(repo.list(mine).await.unwrap().is_empty());
    assert!(repo
        .update(item.id, mine, Some("done".into()), None)
        .await
        .unwrap()
        .is_none());
    assert_eq!(repo.status_of(item.id).as_deref(), Some("queued"));
}
