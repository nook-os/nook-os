//! nook-worker: the process that drains the durable queue (`nook-infra`) and
//! runs a handler per `work_type`.
//!
//! It links `nook-infra` and nothing from the control plane's HTTP/WS/UI
//! surface (MAIN-148 AC-1), so a fleet of these scales cheaply. Delivery is the
//! queue's at-least-once contract: a handler may see the same item twice and
//! **must be idempotent**.
//!
//! Failure is honest (AC-4): a handler that returns `Err` — or panics, which is
//! caught per item so one bad job never crash-loops the worker — is retried with
//! an attempt-scaled backoff (the item is held invisible via `extend_visibility`
//! until the delay elapses, then redelivered). On the **final** attempt the
//! worker dead-letters the item with the handler's actual error (e.g. the SMTP
//! failure for an email send, MAIN-149), so the dead table records the real
//! cause rather than a generic "exhausted".

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use nook_infra::queue::{Nack, Queue, WorkEnvelope};

/// How many items a single receive claims.
const BATCH: usize = 10;
/// Visibility timeout on a claimed item: how long a handler has before the item
/// is considered abandoned and redelivered.
const VISIBILITY: Duration = Duration::from_secs(30);
/// How long to wait after an empty receive before polling again.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Backoff base and ceiling for a failing handler.
const BACKOFF_BASE: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(300);

/// A handler for one `work_type`. Because delivery is at-least-once, `handle`
/// **must be idempotent** — key its effects on `work.id` (or a natural key in
/// the payload) and treat a repeat as a no-op.
#[async_trait::async_trait]
pub trait Handler: Send + Sync {
    async fn handle(&self, work: &WorkEnvelope) -> Result<()>;
}

/// The diagnostic `noop` work type (AC-3): it logs the item and succeeds, so the
/// pipeline can be exercised end to end before any real handler exists.
pub struct NoopHandler;

#[async_trait::async_trait]
impl Handler for NoopHandler {
    async fn handle(&self, work: &WorkEnvelope) -> Result<()> {
        tracing::info!(
            id = %work.id,
            tenant = %work.tenant_id,
            work_type = %work.work_type,
            attempts = work.attempts,
            payload_len = work.payload.len(),
            "noop: work received and acknowledged"
        );
        Ok(())
    }
}

/// The `email.send` handler (MAIN-149): deserialize the rendered message and
/// drive the configured mail provider. It holds a `GuardedMailer`, so the
/// enable / category / quota gates that used to run inline in the control plane
/// run here instead — a held message is a success (acked, not retried); only a
/// transport failure is an error that retries then dead-letters.
pub struct EmailHandler {
    mailer: std::sync::Arc<dyn nook_infra::mailer::Mailer>,
}

impl EmailHandler {
    pub fn new(mailer: std::sync::Arc<dyn nook_infra::mailer::Mailer>) -> Self {
        Self { mailer }
    }
}

#[async_trait::async_trait]
impl Handler for EmailHandler {
    async fn handle(&self, work: &WorkEnvelope) -> Result<()> {
        use nook_infra::mailer::{Category, EmailJob, SendOutcome};
        let job: EmailJob = serde_json::from_slice(&work.payload)
            .map_err(|e| anyhow::anyhow!("bad email.send payload: {e}"))?;
        // A transport failure returns Err → retry/dead-letter. A gate holding the
        // message is a success (Held), logged, and acked — policy, not failure.
        match self
            .mailer
            .send_reporting(
                &job.to,
                &job.subject,
                &job.text_body,
                job.html_body.as_deref(),
                Category::parse(&job.category),
            )
            .await?
        {
            SendOutcome::Delivered => {
                tracing::info!(to = %job.to, subject = %job.subject, "email delivered")
            }
            SendOutcome::Held(reason) => {
                tracing::info!(to = %job.to, reason, "email held — not delivered")
            }
        }
        Ok(())
    }
}

/// A `work_type` → handler map. The set of registered types is also the default
/// receive allow-list.
#[derive(Default, Clone)]
pub struct Registry {
    handlers: HashMap<String, Arc<dyn Handler>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The registry every worker ships with: just `noop` today (AC-3). Real
    /// handlers (email, …) are their own cards (NG-1).
    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.register("noop", Arc::new(NoopHandler));
        r
    }

    pub fn register(
        &mut self,
        work_type: impl Into<String>,
        handler: Arc<dyn Handler>,
    ) -> &mut Self {
        self.handlers.insert(work_type.into(), handler);
        self
    }

    pub fn get(&self, work_type: &str) -> Option<&Arc<dyn Handler>> {
        self.handlers.get(work_type)
    }

    /// Registered types, sorted — the default receive allow-list.
    pub fn types(&self) -> Vec<String> {
        let mut v: Vec<String> = self.handlers.keys().cloned().collect();
        v.sort();
        v
    }
}

/// Resolve the receive allow-list (AC-2): the `NOOK_WORK_TYPES` env value
/// (comma-separated) if set and non-empty, otherwise every registered type.
/// An explicit list may name a type with no handler on purpose — those items
/// are received and then dead-lettered with "no handler".
pub fn resolve_work_types(registry: &Registry) -> Vec<String> {
    match std::env::var("NOOK_WORK_TYPES") {
        Ok(v) if !v.trim().is_empty() => v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => registry.types(),
    }
}

/// Attempt-scaled backoff: `BACKOFF_BASE * 2^(attempts-1)`, capped at
/// `BACKOFF_MAX`. `attempts` is 1 on the first delivery, so the first retry
/// waits `BACKOFF_BASE`.
fn backoff(attempts: i32) -> Duration {
    let shift = attempts.max(1).saturating_sub(1).min(16) as u32;
    let scaled = BACKOFF_BASE.saturating_mul(1u32 << shift);
    scaled.min(BACKOFF_MAX)
}

/// Process one claimed item: dispatch to its handler, or handle the no-handler
/// and failure cases. Never panics — a handler panic is caught and treated as a
/// failure (AC-4).
async fn process(queue: &dyn Queue, registry: &Registry, work: &WorkEnvelope) -> Result<()> {
    let Some(handler) = registry.get(&work.work_type) else {
        // AC-2: a received type with no handler is requeued once, then
        // dead-lettered. `attempts` is the delivery count; on the first it is 1
        // (requeue), on the second it is 2 (dead-letter).
        if work.attempts <= 1 {
            tracing::warn!(id = %work.id, work_type = %work.work_type, "no handler — requeueing once");
            queue.nack(work.id, Nack::Requeue).await?;
        } else {
            tracing::error!(id = %work.id, work_type = %work.work_type, "no handler — dead-lettering");
            queue
                .nack(
                    work.id,
                    Nack::Dead(format!("no handler for work_type {:?}", work.work_type)),
                )
                .await?;
        }
        return Ok(());
    };

    // Run the handler on its own task so a panic surfaces as a JoinError instead
    // of unwinding the receive loop — one poison job must never crash-loop the
    // worker (AC-4).
    let handler = handler.clone();
    let owned = work.clone();
    let outcome = tokio::spawn(async move { handler.handle(&owned).await }).await;

    match outcome {
        Ok(Ok(())) => {
            queue.ack(work.id).await?;
        }
        Ok(Err(e)) => {
            fail(queue, work, &e.to_string()).await?;
        }
        Err(join) => {
            let reason = if join.is_panic() {
                "handler panicked".to_string()
            } else {
                format!("handler task failed: {join}")
            };
            fail(queue, work, &reason).await?;
        }
    }
    Ok(())
}

/// A handler failed (returned `Err` or panicked). Retry with an attempt-scaled
/// backoff, but on the **final** attempt dead-letter with the actual `reason` —
/// so a persistent failure lands in the dead table with its real cause (e.g. the
/// SMTP error for an email send, MAIN-149 AC-3) rather than a generic
/// "exhausted".
async fn fail(queue: &dyn Queue, work: &WorkEnvelope, reason: &str) -> Result<()> {
    if work.attempts >= work.max_attempts {
        tracing::error!(id = %work.id, reason, attempts = work.attempts, "handler failed — dead-lettering");
        queue.nack(work.id, Nack::Dead(reason.to_string())).await?;
    } else {
        let delay = backoff(work.attempts);
        tracing::warn!(id = %work.id, reason, backoff_secs = delay.as_secs(), "handler failed — backing off");
        queue.extend_visibility(work.id, delay).await?;
    }
    Ok(())
}

/// Claim and process one batch. Returns how many items were handled (0 when the
/// queue is idle). Exposed for tests and for the loop below.
pub async fn drain_once(queue: &dyn Queue, registry: &Registry, types: &[String]) -> Result<usize> {
    let batch = queue.receive(types, BATCH, VISIBILITY).await?;
    let n = batch.len();
    for work in &batch {
        process(queue, registry, work).await?;
    }
    Ok(n)
}

/// The receive loop. Drains batches until `shutdown` flips true; a shutdown that
/// arrives mid-batch lets the current batch finish before the next iteration
/// exits, so in-flight work completes rather than being abandoned (AC-1).
pub async fn run(
    queue: Arc<dyn Queue>,
    registry: Registry,
    types: Vec<String>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    tracing::info!(?types, "worker draining");
    loop {
        if *shutdown.borrow_and_update() {
            break;
        }
        let handled = drain_once(&*queue, &registry, &types).await?;
        if handled == 0 {
            // Idle: wait a beat, but wake immediately on shutdown.
            tokio::select! {
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
                _ = shutdown.changed() => {}
            }
        }
    }
    tracing::info!("worker stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nook_infra::queue::NewWork;
    use nook_testkit::TestBed;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use uuid::Uuid;

    // A private, migrated database (MAIN-166) plus a queue built on it. Holding
    // the `TestBed` keeps the DB alive for the test; its Drop drops the DB.
    async fn setup() -> Option<(TestBed, Arc<dyn Queue>)> {
        let bed = TestBed::new().await?;
        let cfg = nook_infra::Config::for_test();
        let q: Arc<dyn Queue> =
            Arc::from(nook_infra::queue::from_config(&cfg, bed.pool.clone()).await);
        Some((bed, q))
    }

    fn unique_type(tag: &str) -> String {
        format!("test.worker.{tag}.{}", Uuid::now_v7())
    }

    fn work(ty: &str) -> NewWork {
        NewWork::new(Uuid::now_v7(), ty, b"{}".to_vec())
    }

    // A handler that records every id it sees.
    struct Recorder(Arc<std::sync::Mutex<Vec<Uuid>>>);
    #[async_trait::async_trait]
    impl Handler for Recorder {
        async fn handle(&self, work: &WorkEnvelope) -> Result<()> {
            self.0.lock().unwrap().push(work.id);
            Ok(())
        }
    }

    struct Failing(Arc<AtomicUsize>);
    #[async_trait::async_trait]
    impl Handler for Failing {
        async fn handle(&self, _work: &WorkEnvelope) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("handler always fails")
        }
    }

    struct Panicking;
    #[async_trait::async_trait]
    impl Handler for Panicking {
        async fn handle(&self, _work: &WorkEnvelope) -> Result<()> {
            panic!("boom");
        }
    }

    async fn count_in(pool: &sqlx::PgPool, table: &str, ty: &str) -> i64 {
        sqlx::query_as::<_, (i64,)>(&format!(
            "SELECT count(*) FROM {table} WHERE work_type = $1"
        ))
        .bind(ty)
        .fetch_one(pool)
        .await
        .unwrap()
        .0
    }

    #[test]
    fn backoff_scales_with_attempts_and_caps() {
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(2), Duration::from_secs(4));
        assert_eq!(backoff(3), Duration::from_secs(8));
        assert_eq!(backoff(100), BACKOFF_MAX, "capped");
    }

    #[test]
    fn allow_list_falls_back_to_registered_types() {
        std::env::remove_var("NOOK_WORK_TYPES");
        let mut r = Registry::new();
        r.register("a", Arc::new(NoopHandler));
        r.register("b", Arc::new(NoopHandler));
        assert_eq!(
            resolve_work_types(&r),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[tokio::test]
    async fn dispatches_to_the_registered_handler_and_acks() {
        nook_testkit::deadline(60, async {
            let Some((_bed, q)) = setup().await else {
                return;
            };
            let ty = unique_type("dispatch");
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let mut reg = Registry::new();
            reg.register(ty.clone(), Arc::new(Recorder(seen.clone())));

            let id = q.enqueue(work(&ty)).await.unwrap();
            let n = drain_once(&*q, &reg, std::slice::from_ref(&ty))
                .await
                .unwrap();
            assert_eq!(n, 1);
            assert_eq!(
                seen.lock().unwrap().as_slice(),
                &[id],
                "handler saw the item"
            );

            // Acked → gone: a second drain finds nothing.
            let again = drain_once(&*q, &reg, std::slice::from_ref(&ty))
                .await
                .unwrap();
            assert_eq!(again, 0, "the item was acked");
        })
        .await;
    }

    #[tokio::test]
    async fn allow_list_filters_which_types_are_received() {
        nook_testkit::deadline(60, async {
            let Some((_bed, q)) = setup().await else {
                return;
            };
            let a = unique_type("allowA");
            let b = unique_type("allowB");
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let mut reg = Registry::new();
            reg.register(a.clone(), Arc::new(Recorder(seen.clone())));
            reg.register(b.clone(), Arc::new(Recorder(seen.clone())));

            let ida = q.enqueue(work(&a)).await.unwrap();
            let _idb = q.enqueue(work(&b)).await.unwrap();

            // Drain only type A.
            let n = drain_once(&*q, &reg, std::slice::from_ref(&a))
                .await
                .unwrap();
            assert_eq!(n, 1);
            assert_eq!(
                seen.lock().unwrap().as_slice(),
                &[ida],
                "only A was received"
            );

            // B is still queued.
            let n_b = drain_once(&*q, &reg, std::slice::from_ref(&b))
                .await
                .unwrap();
            assert_eq!(n_b, 1, "B was left untouched by the A-only drain");
        })
        .await;
    }

    #[tokio::test]
    async fn unregistered_type_is_requeued_once_then_dead_lettered() {
        nook_testkit::deadline(60, async {
            let Some((bed, q)) = setup().await else {
                return;
            };
            let ty = unique_type("nohandler");
            // An allow-list that names a type with no registered handler.
            let reg = Registry::new();
            q.enqueue(work(&ty)).await.unwrap();

            // First delivery (attempts=1): requeued, not dead yet.
            drain_once(&*q, &reg, std::slice::from_ref(&ty))
                .await
                .unwrap();
            assert_eq!(
                count_in(&bed.pool, "work_queue_dead", &ty).await,
                0,
                "not dead on the first pass"
            );
            assert_eq!(
                count_in(&bed.pool, "work_queue", &ty).await,
                1,
                "back in the queue"
            );

            // Second delivery (attempts=2): dead-lettered with "no handler".
            drain_once(&*q, &reg, std::slice::from_ref(&ty))
                .await
                .unwrap();
            assert_eq!(
                count_in(&bed.pool, "work_queue", &ty).await,
                0,
                "left the live queue"
            );
            assert_eq!(
                count_in(&bed.pool, "work_queue_dead", &ty).await,
                1,
                "dead-lettered"
            );
            let reason: String = sqlx::query_as::<_, (String,)>(
                "SELECT reason FROM work_queue_dead WHERE work_type = $1",
            )
            .bind(&ty)
            .fetch_one(&bed.pool)
            .await
            .unwrap()
            .0;
            assert!(
                reason.contains("no handler"),
                "reason names the cause: {reason}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn handler_failure_backs_off_without_acking() {
        nook_testkit::deadline(60, async {
            let Some((bed, q)) = setup().await else {
                return;
            };
            let ty = unique_type("fail");
            let calls = Arc::new(AtomicUsize::new(0));
            let mut reg = Registry::new();
            reg.register(ty.clone(), Arc::new(Failing(calls.clone())));

            let id = q.enqueue(work(&ty)).await.unwrap();
            drain_once(&*q, &reg, std::slice::from_ref(&ty))
                .await
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1, "handler ran once");

            // Not acked, not dead — still present, and invisible into the future.
            assert_eq!(
                count_in(&bed.pool, "work_queue", &ty).await,
                1,
                "still queued"
            );
            assert_eq!(
                count_in(&bed.pool, "work_queue_dead", &ty).await,
                0,
                "not dead after one failure"
            );
            let locked_future: bool = sqlx::query_as::<_, (bool,)>(
                "SELECT locked_until > now() FROM work_queue WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&bed.pool)
            .await
            .unwrap()
            .0;
            assert!(locked_future, "the item is held invisible by the backoff");
        })
        .await;
    }

    #[tokio::test]
    async fn a_panicking_handler_is_isolated_not_fatal() {
        nook_testkit::deadline(60, async {
            let Some((bed, q)) = setup().await else {
                return;
            };
            let ty = unique_type("panic");
            let mut reg = Registry::new();
            reg.register(ty.clone(), Arc::new(Panicking));

            q.enqueue(work(&ty)).await.unwrap();
            // The panic must be caught: drain_once returns Ok, not a propagated unwind.
            let n = drain_once(&*q, &reg, std::slice::from_ref(&ty))
                .await
                .unwrap();
            assert_eq!(n, 1);
            // Treated as a failure: backed off, still present, not dead.
            assert_eq!(
                count_in(&bed.pool, "work_queue", &ty).await,
                1,
                "a panic is a failure, not a loss"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn run_drains_pending_work_then_stops_on_shutdown() {
        nook_testkit::deadline(60, async {
            let Some((bed, q)) = setup().await else {
                return;
            };
            let ty = unique_type("shutdown");
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let mut reg = Registry::new();
            reg.register(ty.clone(), Arc::new(Recorder(seen.clone())));

            for _ in 0..3 {
                q.enqueue(work(&ty)).await.unwrap();
            }

            let (tx, rx) = tokio::sync::watch::channel(false);
            let handle = tokio::spawn(run(q.clone(), reg, vec![ty.clone()], rx));

            // Wait until the three items are drained, then signal shutdown.
            for _ in 0..50 {
                if count_in(&bed.pool, "work_queue", &ty).await == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            tx.send(true).unwrap();

            tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .expect("run returns promptly after shutdown")
                .unwrap()
                .unwrap();
            assert_eq!(
                seen.lock().unwrap().len(),
                3,
                "all pending work was drained before exit"
            );
        })
        .await;
    }

    // ── MAIN-149: the email.send handler + dead-letter-with-reason ──────────────

    #[tokio::test]
    async fn email_handler_delivers_via_the_mailer_and_acks() {
        nook_testkit::deadline(60, async {
            let Some((_bed, q)) = setup().await else {
                return;
            };
            use nook_infra::mailer::{capture::CaptureMailer, Category, EmailJob};
            let ty = unique_type("email");
            let cap = Arc::new(CaptureMailer::new());
            let mut reg = Registry::new();
            reg.register(ty.clone(), Arc::new(EmailHandler::new(cap.clone())));

            let job = EmailJob::new(
                "to@x.test",
                "Subject",
                "the body",
                None,
                Category::Transactional,
            );
            let payload = serde_json::to_vec(&job).unwrap();
            q.enqueue(NewWork::new(Uuid::now_v7(), ty.clone(), payload))
                .await
                .unwrap();

            let n = drain_once(&*q, &reg, std::slice::from_ref(&ty))
                .await
                .unwrap();
            assert_eq!(n, 1);
            assert_eq!(
                cap.sent().len(),
                1,
                "the mailer delivered the queued message"
            );
            // Acked → a second drain finds nothing.
            assert_eq!(
                drain_once(&*q, &reg, std::slice::from_ref(&ty))
                    .await
                    .unwrap(),
                0,
                "a delivered email is acked"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_failing_handler_dead_letters_with_its_error_on_the_final_attempt() {
        nook_testkit::deadline(60, async {
            let Some((bed, q)) = setup().await else {
                return;
            };
            let ty = unique_type("deadfail");
            let mut reg = Registry::new();
            reg.register(ty.clone(), Arc::new(Failing(Arc::new(AtomicUsize::new(0)))));

            // max_attempts = 1: the first delivery is also the last, so a failure
            // dead-letters immediately — carrying the handler's actual error (AC-3).
            q.enqueue(NewWork::new(Uuid::now_v7(), ty.clone(), b"{}".to_vec()).max_attempts(1))
                .await
                .unwrap();
            drain_once(&*q, &reg, std::slice::from_ref(&ty))
                .await
                .unwrap();

            assert_eq!(
                count_in(&bed.pool, "work_queue", &ty).await,
                0,
                "left the live queue"
            );
            let reason: String = sqlx::query_as::<_, (String,)>(
                "SELECT reason FROM work_queue_dead WHERE work_type = $1",
            )
            .bind(&ty)
            .fetch_one(&bed.pool)
            .await
            .unwrap()
            .0;
            assert!(
                reason.contains("handler always fails"),
                "the dead-letter reason is the handler's error, not a generic one: {reason}"
            );
        })
        .await;
    }
}
