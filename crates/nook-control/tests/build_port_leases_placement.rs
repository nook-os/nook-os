//! A build run leases the ports it binds — the PLACEMENT half (MAIN-552).
//!
//! Separated from `build_port_leases` because of what it drives, not what it
//! asserts: every test here goes through `jobs::select_executor`, which asks
//! `eligible_loop_executors`. That query read `json_each(…) e`'s fields off the
//! alias, so this binary was allow-listed beside `executor_selection` and
//! `dispatch_order`, which waited on the same card. MAIN-546 fixed it and all
//! four lines are gone, so both halves are covered on both engines; the split
//! stays because what each drives is still different.
//!
//! What is here and nowhere else: the gate that decides a build WAITS rather
//! than fails when a required listener has no port, and that the wait is one
//! the starvation sweep can end.

mod common;

use common::build_ports::*;
use nook_control::services::jobs;
use nook_testkit::TestBed;
use nook_types::*;

/// AC-6: an unsatisfiable REQUIRED listener queues the job with a typed reason
/// naming the listener — it does not fail, and it does not run unleased.
#[tokio::test]
async fn an_unsatisfiable_required_listener_queues_rather_than_fails() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed, "bplp-queue").await;
    declare(&bed, &f, &[("web", "NOOK_WEB_PORT", true)]).await;
    // A range of exactly one port, taken by a card that got there first.
    let node = build_node(&bed, &f, Some((4200, 4200))).await;
    let todo = column(&bed, &f, "Todo", "unstarted", 0).await;
    let state = bed.app_state().await;

    let first = card(&bed, &f, todo, 1).await;
    let first_job = queued_build_job(&bed, &f, first).await;
    jobs::select_executor(&state, f.tenant, first_job)
        .await
        .expect("first");
    assert_eq!(
        held(&state, node, first).await.len(),
        1,
        "the range is now full"
    );

    let second = card(&bed, &f, todo, 2).await;
    let second_job = queued_build_job(&bed, &f, second).await;
    let placed = jobs::select_executor(&state, f.tenant, second_job)
        .await
        .expect("second");

    assert_eq!(
        placed.state, "queued",
        "a shortage is a wait, not a failure"
    );
    assert_eq!(
        placed.queued_reason_kind,
        Some(QueuedReason::PortsUnavailable {
            listener: "web".into(),
            env: "NOOK_WEB_PORT".into(),
        }),
        "MAIN-494's typed reason, naming what a human would have to free"
    );
    let reason = placed.queued_reason.clone().unwrap_or_default();
    assert!(
        reason.contains("port") && reason.contains("web"),
        "the sentence names ports and the listener: {reason}"
    );
    assert!(
        held(&state, node, second).await.is_empty(),
        "a refused build holds nothing — a half-leased set would be worse than none"
    );

    bed.teardown().await;
}

/// A build that IS satisfiable places, and the leases are taken by the time it
/// is claimed — before anything could dispatch and start binding.
#[tokio::test]
async fn a_placed_build_holds_its_ports_by_the_time_it_is_claimed() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed, "bplp-place").await;
    declare(
        &bed,
        &f,
        &[
            ("web", "NOOK_WEB_PORT", true),
            ("api", "NOOK_API_PORT", true),
        ],
    )
    .await;
    let node = build_node(&bed, &f, Some((4200, 4210))).await;
    let todo = column(&bed, &f, "Todo", "unstarted", 0).await;
    let task = card(&bed, &f, todo, 1).await;
    let job = queued_build_job(&bed, &f, task).await;
    let state = bed.app_state().await;

    let placed = jobs::select_executor(&state, f.tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "claimed", "the build should place");
    assert_eq!(placed.executor_node_id, Some(node));
    assert_eq!(
        held(&state, node, task).await,
        vec![
            ("NOOK_API_PORT".to_string(), 4201),
            ("NOOK_WEB_PORT".to_string(), 4200),
        ],
        "leased on the node it was placed on, under the workspace's own names"
    );

    bed.teardown().await;
}

/// A node advertising no range still takes build work: not every machine
/// publishes ports, and that is not a reason to refuse a run.
#[tokio::test]
async fn a_node_with_no_range_still_runs_builds() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed, "bplp-norange").await;
    let node = build_node(&bed, &f, None).await;
    let todo = column(&bed, &f, "Todo", "unstarted", 0).await;
    let task = card(&bed, &f, todo, 1).await;
    let job = queued_build_job(&bed, &f, task).await;
    let state = bed.app_state().await;

    let placed = jobs::select_executor(&state, f.tenant, job)
        .await
        .expect("select");
    assert_eq!(placed.state, "claimed");
    assert!(held(&state, node, task).await.is_empty());

    bed.teardown().await;
}

/// AC-7: "waiting for a port that never frees" must not become a silent
/// forever-wait. A job queued on ports is reached by the starvation escalation
/// exactly like any other queued job.
#[tokio::test]
async fn a_job_queued_on_ports_is_still_reachable_by_the_starvation_sweep() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed, "bplp-starved").await;
    declare(&bed, &f, &[("web", "NOOK_WEB_PORT", true)]).await;
    let _node = build_node(&bed, &f, Some((4200, 4200))).await;
    let todo = column(&bed, &f, "Todo", "unstarted", 0).await;
    let state = bed.app_state().await;

    let first = card(&bed, &f, todo, 1).await;
    let first_job = queued_build_job(&bed, &f, first).await;
    jobs::select_executor(&state, f.tenant, first_job)
        .await
        .expect("first");

    let starved = card(&bed, &f, todo, 2).await;
    let starved_job = queued_build_job(&bed, &f, starved).await;
    let placed = jobs::select_executor(&state, f.tenant, starved_job)
        .await
        .expect("second");
    assert_eq!(placed.state, "queued");

    // `0` seconds: anything queued with a reason is past the threshold, which
    // is what makes this about REACHABILITY rather than about the clock.
    let ended = jobs::escalate_starved_queued(&state, f.tenant, 0)
        .await
        .expect("escalate");
    assert_eq!(ended, 1, "the port wait is escalatable like any other");

    let after = state
        .jobs
        .get(f.tenant, starved_job)
        .await
        .expect("read back")
        .expect("job");
    assert_eq!(after.state, "canceled", "and it does not wait forever");

    bed.teardown().await;
}
