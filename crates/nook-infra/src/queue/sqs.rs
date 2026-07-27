//! The Amazon SQS queue backend (MAIN-152).
//!
//! SQS is a managed queue, so most of the [`Queue`](super::Queue) contract maps
//! to a native call: the visibility timeout is SQS's own, `ack` is
//! `DeleteMessage`, `nack(Requeue)` is `ChangeMessageVisibility(0)`, and
//! `extend_visibility` is `ChangeMessageVisibility(n)`. Delivery is
//! at-least-once, exactly as the module contract requires.
//!
//! Three places where SQS's model and this trait's model differ, handled
//! honestly rather than papered over:
//!
//! - **The trait keys on our `Uuid`; SQS keys on a per-receive receipt handle.**
//!   `ack`/`nack`/`extend` need the handle, which only the `receive` that
//!   produced it knows. So each `receive` records `id → handle` in an in-process
//!   map, and the mutating calls consume it. A handle is valid only inside the
//!   visibility window — precisely this map's useful lifetime — so nothing is
//!   lost on a restart that a redelivery does not fix.
//!
//! - **SQS has no server-side receive filter.** `receive(types)` therefore reads
//!   each message's `nook-type` attribute and keeps only the requested types,
//!   *releasing* the rest (visibility → 0) so their own consumer still gets them.
//!   On a queue shared by many types this is best-effort and a little wasteful;
//!   the honest fix for high-fan-out is one queue per type. A bounded
//!   no-progress cutoff keeps the release loop from spinning on foreign traffic.
//!
//! - **SQS dead-letters via a queue-level redrive policy, not per message.** The
//!   trait's per-message `max_attempts` is enforced here in the app: `receive`
//!   compares `ApproximateReceiveCount` (which is also the reported `attempts`)
//!   to the message's `nook-max` and, when exceeded, dead-letters it. An
//!   explicit `Nack::Dead` does the same. Dead-lettering sends the message to the
//!   configured DLQ (with its reason as an attribute) and deletes it from the
//!   main queue; with no DLQ configured it is deleted and the reason logged. An
//!   operator-configured redrive policy on the main queue is the backstop
//!   (documented, not created by us — NG-1).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use aws_sdk_sqs::primitives::Blob;
use aws_sdk_sqs::types::{MessageAttributeValue, MessageSystemAttributeName, QueueAttributeName};
use aws_sdk_sqs::Client;
use chrono::{DateTime, TimeZone, Utc};
use uuid::Uuid;

use super::{Nack, NewWork, Queue, QueueStats, WorkEnvelope, DEFAULT_MAX_ATTEMPTS};

/// Our metadata rides as SQS message attributes; the opaque payload rides as a
/// binary attribute so its exact bytes survive (the SQS body must be UTF-8).
const ATTR_ID: &str = "nook-id";
const ATTR_TYPE: &str = "nook-type";
const ATTR_TENANT: &str = "nook-tenant";
const ATTR_MAX: &str = "nook-max-attempts";
const ATTR_PAYLOAD: &str = "nook-payload";
const ATTR_REASON: &str = "nook-dead-reason";

/// Give up releasing foreign-type messages after this many consecutive batches
/// that claimed nothing of a requested type — bounds the receive-side filter on
/// a shared queue so it can't spin.
const NO_PROGRESS_CUTOFF: u32 = 2;

/// How long a `receive` long-polls SQS per batch (seconds). Long polling avoids
/// the empty-response churn of short polling and lets a batch drain what's there.
const WAIT_SECS: i32 = 1;

pub struct SqsQueue {
    client: Client,
    queue_url: String,
    dlq_url: Option<String>,
    in_flight: Mutex<HashMap<Uuid, InFlight>>,
}

/// What a claimed message keeps between `receive` and its `ack`/`nack`: the
/// receipt handle SQS needs, plus the fields a later `Nack::Dead` must replay to
/// the DLQ (the trait's `nack(id)` carries none of them).
#[derive(Clone)]
struct InFlight {
    handle: String,
    work_type: String,
    tenant_id: Uuid,
    payload: Vec<u8>,
    max_attempts: i32,
}

impl SqsQueue {
    pub fn new(client: Client, queue_url: String, dlq_url: Option<String>) -> Self {
        Self {
            client,
            queue_url,
            dlq_url,
            in_flight: Mutex::new(HashMap::new()),
        }
    }

    /// Build the SQS queue from config: region + optional endpoint override (an
    /// emulator or VPC endpoint) + the standard AWS credential chain. Probes the
    /// queue with a `GetQueueAttributes` so a missing/unreachable queue fails at
    /// boot rather than at the first drain three days later (AC-3). Mirrors
    /// `S3Store::from_config`.
    pub async fn from_config(cfg: &crate::config::Config) -> Result<Self> {
        let queue_url = cfg
            .sqs_queue_url
            .clone()
            .filter(|u| !u.is_empty())
            .context("NOOK_SQS_QUEUE_URL is required when NOOK_QUEUE_PROVIDER=sqs")?;
        let region = cfg
            .sqs_region
            .clone()
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| "us-east-1".into());

        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region));
        if let Some(ep) = cfg.sqs_endpoint_url.clone().filter(|e| !e.is_empty()) {
            loader = loader.endpoint_url(ep);
        }
        let shared = loader.load().await;
        let client = Client::new(&shared);

        let dlq_url = cfg.sqs_dlq_url.clone().filter(|u| !u.is_empty());
        let q = Self::new(client, queue_url, dlq_url);
        q.client
            .get_queue_attributes()
            .queue_url(&q.queue_url)
            .attribute_names(QueueAttributeName::QueueArn)
            .send()
            .await
            .with_context(|| format!("cannot reach the SQS queue at {}", q.queue_url))?;
        Ok(q)
    }

    pub fn backend(&self) -> &'static str {
        "sqs"
    }

    /// A `String` message attribute.
    fn str_attr(v: impl Into<String>) -> MessageAttributeValue {
        MessageAttributeValue::builder()
            .data_type("String")
            .string_value(v)
            .build()
            .expect("String attribute has its required data_type")
    }

    /// A `Number` message attribute.
    fn num_attr(n: i64) -> MessageAttributeValue {
        MessageAttributeValue::builder()
            .data_type("Number")
            .string_value(n.to_string())
            .build()
            .expect("Number attribute has its required data_type")
    }

    /// A `Binary` message attribute (the opaque payload, exact bytes).
    fn bin_attr(bytes: Vec<u8>) -> MessageAttributeValue {
        MessageAttributeValue::builder()
            .data_type("Binary")
            .binary_value(Blob::new(bytes))
            .build()
            .expect("Binary attribute has its required data_type")
    }

    /// Consume the stored in-flight record for `id` (removes it — the mutating
    /// op that follows retires this delivery).
    fn take(&self, id: Uuid) -> Option<InFlight> {
        self.in_flight.lock().unwrap().remove(&id)
    }

    /// Release a fetched message immediately so its own consumer sees it.
    async fn release(&self, receipt_handle: &str) {
        let _ = self
            .client
            .change_message_visibility()
            .queue_url(&self.queue_url)
            .receipt_handle(receipt_handle)
            .visibility_timeout(0)
            .send()
            .await;
    }

    /// Move a message to the DLQ (if configured) with its reason, then delete it
    /// from the main queue. With no DLQ, delete and log the reason.
    async fn dead_letter(&self, id: Uuid, f: &InFlight, reason: &str) -> Result<()> {
        if let Some(dlq) = &self.dlq_url {
            self.client
                .send_message()
                .queue_url(dlq)
                .message_body("-")
                .message_attributes(ATTR_ID, Self::str_attr(id.to_string()))
                .message_attributes(ATTR_TYPE, Self::str_attr(f.work_type.clone()))
                .message_attributes(ATTR_TENANT, Self::str_attr(f.tenant_id.to_string()))
                .message_attributes(ATTR_MAX, Self::num_attr(f.max_attempts as i64))
                .message_attributes(ATTR_REASON, Self::str_attr(reason))
                .message_attributes(ATTR_PAYLOAD, Self::bin_attr(f.payload.clone()))
                .send()
                .await
                .context("sqs dead-letter send")?;
        } else {
            tracing::warn!(
                %id,
                work_type = %f.work_type,
                reason,
                "sqs message dead-lettered but no NOOK_SQS_DLQ_URL configured — dropping"
            );
        }
        self.client
            .delete_message()
            .queue_url(&self.queue_url)
            .receipt_handle(&f.handle)
            .send()
            .await
            .context("sqs dead-letter delete")?;
        Ok(())
    }
}

fn str_of(m: &aws_sdk_sqs::types::Message, key: &str) -> Option<String> {
    m.message_attributes()?
        .get(key)
        .and_then(|a| a.string_value())
        .map(|s| s.to_string())
}

fn bin_of(m: &aws_sdk_sqs::types::Message, key: &str) -> Vec<u8> {
    m.message_attributes()
        .and_then(|a| a.get(key))
        .and_then(|a| a.binary_value())
        .map(|b| b.as_ref().to_vec())
        .unwrap_or_default()
}

fn receive_count(m: &aws_sdk_sqs::types::Message) -> i32 {
    m.attributes()
        .and_then(|a| a.get(&MessageSystemAttributeName::ApproximateReceiveCount))
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
}

fn sent_at(m: &aws_sdk_sqs::types::Message) -> DateTime<Utc> {
    m.attributes()
        .and_then(|a| a.get(&MessageSystemAttributeName::SentTimestamp))
        .and_then(|s| s.parse::<i64>().ok())
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
        .unwrap_or_else(Utc::now)
}

#[async_trait::async_trait]
impl Queue for SqsQueue {
    async fn enqueue(&self, work: NewWork) -> Result<Uuid> {
        let id = Uuid::now_v7();
        let mut req = self
            .client
            .send_message()
            .queue_url(&self.queue_url)
            .message_body("-")
            .message_attributes(ATTR_ID, Self::str_attr(id.to_string()))
            .message_attributes(ATTR_TYPE, Self::str_attr(work.work_type.clone()))
            .message_attributes(ATTR_TENANT, Self::str_attr(work.tenant_id.to_string()))
            .message_attributes(ATTR_MAX, Self::num_attr(work.max_attempts as i64))
            .message_attributes(ATTR_PAYLOAD, Self::bin_attr(work.payload));
        if let Some(delay) = work.delay {
            // SQS delay is integer seconds, capped at 900 (15 min).
            req = req.delay_seconds((delay.as_secs() as i32).clamp(0, 900));
        }
        req.send().await.context("sqs enqueue")?;
        Ok(id)
    }

    async fn receive(
        &self,
        types: &[String],
        max: usize,
        visibility: Duration,
    ) -> Result<Vec<WorkEnvelope>> {
        let vis_secs = visibility.as_secs() as i32;
        let mut out: Vec<WorkEnvelope> = Vec::new();
        let mut no_progress = 0u32;
        let mut first_batch = true;

        while out.len() < max && no_progress < NO_PROGRESS_CUTOFF {
            let want = ((max - out.len()).min(10)) as i32;
            // Long-poll only the first batch — to wait for work to arrive. Once
            // we hold some, later batches short-poll (drain what's immediately
            // there) so `receive` returns promptly instead of idling a full poll,
            // which also keeps the call shorter than a short visibility window.
            let wait = if first_batch { WAIT_SECS } else { 0 };
            first_batch = false;
            let resp = self
                .client
                .receive_message()
                .queue_url(&self.queue_url)
                .max_number_of_messages(want)
                .visibility_timeout(vis_secs)
                .wait_time_seconds(wait)
                .message_attribute_names("All")
                .message_system_attribute_names(MessageSystemAttributeName::ApproximateReceiveCount)
                .message_system_attribute_names(MessageSystemAttributeName::SentTimestamp)
                .send()
                .await
                .context("sqs receive")?;

            let msgs = resp.messages();
            if msgs.is_empty() {
                break; // nothing visible within the long-poll window
            }

            let mut claimed = 0;
            for m in msgs {
                let handle = match m.receipt_handle() {
                    Some(h) => h,
                    None => continue,
                };
                let work_type = str_of(m, ATTR_TYPE).unwrap_or_default();

                // Receive-side type filter: release anything we were not asked for.
                if !types.is_empty() && !types.iter().any(|t| t == &work_type) {
                    self.release(handle).await;
                    continue;
                }

                let attempts = receive_count(m);
                let max_attempts = str_of(m, ATTR_MAX)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(DEFAULT_MAX_ATTEMPTS);
                let id = str_of(m, ATTR_ID)
                    .and_then(|s| Uuid::parse_str(&s).ok())
                    .unwrap_or_else(Uuid::now_v7);
                let tenant_id = str_of(m, ATTR_TENANT)
                    .and_then(|s| Uuid::parse_str(&s).ok())
                    .unwrap_or(Uuid::nil());
                let payload = bin_of(m, ATTR_PAYLOAD);
                let record = InFlight {
                    handle: handle.to_string(),
                    work_type: work_type.clone(),
                    tenant_id,
                    payload: payload.clone(),
                    max_attempts,
                };

                // App-level attempt cap (SQS enforces via redrive policy, not
                // per message): past the cap, dead-letter instead of delivering.
                if attempts > max_attempts {
                    self.dead_letter(id, &record, "max attempts exhausted")
                        .await?;
                    continue;
                }

                self.in_flight.lock().unwrap().insert(id, record);
                out.push(WorkEnvelope {
                    id,
                    tenant_id,
                    work_type,
                    payload,
                    attempts,
                    max_attempts,
                    not_before: Utc::now(),
                    enqueued_at: sent_at(m),
                });
                claimed += 1;
            }

            if claimed == 0 {
                no_progress += 1;
            } else {
                no_progress = 0;
            }
        }
        Ok(out)
    }

    async fn ack(&self, id: Uuid) -> Result<()> {
        // Acking an id we hold no handle for is a no-op, matching the trait.
        if let Some(f) = self.take(id) {
            self.client
                .delete_message()
                .queue_url(&self.queue_url)
                .receipt_handle(f.handle)
                .send()
                .await
                .context("sqs ack delete")?;
        }
        Ok(())
    }

    async fn nack(&self, id: Uuid, disposition: Nack) -> Result<()> {
        let Some(f) = self.take(id) else {
            return Ok(());
        };
        match disposition {
            Nack::Requeue => {
                self.client
                    .change_message_visibility()
                    .queue_url(&self.queue_url)
                    .receipt_handle(&f.handle)
                    .visibility_timeout(0)
                    .send()
                    .await
                    .context("sqs nack requeue")?;
            }
            Nack::Dead(reason) => {
                // We retained the message's attributes at receive, so the DLQ
                // record carries its type/tenant/payload — dead_count(type) and
                // dead_reason(id) both resolve.
                self.dead_letter(id, &f, &reason).await?;
            }
        }
        Ok(())
    }

    async fn extend_visibility(&self, id: Uuid, visibility: Duration) -> Result<()> {
        let handle = self
            .in_flight
            .lock()
            .unwrap()
            .get(&id)
            .map(|f| f.handle.clone());
        if let Some(handle) = handle {
            self.client
                .change_message_visibility()
                .queue_url(&self.queue_url)
                .receipt_handle(handle)
                .visibility_timeout(visibility.as_secs() as i32)
                .send()
                .await
                .context("sqs extend_visibility")?;
        }
        Ok(())
    }

    async fn describe(&self) -> Result<QueueStats> {
        let a = self
            .client
            .get_queue_attributes()
            .queue_url(&self.queue_url)
            .attribute_names(QueueAttributeName::ApproximateNumberOfMessages)
            .attribute_names(QueueAttributeName::ApproximateNumberOfMessagesNotVisible)
            .send()
            .await
            .context("sqs describe")?;
        let get = |k: &QueueAttributeName| -> i64 {
            a.attributes()
                .and_then(|m| m.get(k))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
        };
        let ready = get(&QueueAttributeName::ApproximateNumberOfMessages);
        let in_flight = get(&QueueAttributeName::ApproximateNumberOfMessagesNotVisible);

        let dead = if let Some(dlq) = &self.dlq_url {
            self.client
                .get_queue_attributes()
                .queue_url(dlq)
                .attribute_names(QueueAttributeName::ApproximateNumberOfMessages)
                .send()
                .await
                .ok()
                .and_then(|r| {
                    r.attributes()
                        .and_then(|m| m.get(&QueueAttributeName::ApproximateNumberOfMessages))
                        .and_then(|s| s.parse().ok())
                })
                .unwrap_or(0)
        } else {
            0
        };

        Ok(QueueStats {
            backend: self.backend().into(),
            ready,
            in_flight,
            dead,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::contract::{self, DeadInspect};

    /// Build a client against the test SQS emulator (ElasticMQ by default; set
    /// `NOOK_TEST_SQS_ENDPOINT` to point elsewhere). Static creds — the emulator
    /// ignores them, and this keeps the tests off any real AWS credential chain.
    async fn client() -> Option<Client> {
        let endpoint = std::env::var("NOOK_TEST_SQS_ENDPOINT")
            .unwrap_or_else(|_| "http://localhost:9324".to_string());
        let shared = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new("us-east-1"))
            .endpoint_url(&endpoint)
            .credentials_provider(aws_credential_types::Credentials::new(
                "test",
                "test",
                None,
                None,
                "nook-test",
            ))
            .load()
            .await;
        let client = Client::new(&shared);
        // Reachability gate: skip cleanly if no emulator is running.
        client.list_queues().send().await.ok()?;
        Some(client)
    }

    async fn create_queue(client: &Client, name: &str) -> String {
        client
            .create_queue()
            .queue_name(name)
            .send()
            .await
            .expect("create test queue")
            .queue_url()
            .expect("queue url")
            .to_string()
    }

    /// A fresh main queue + DLQ per test, so no cross-test type interference —
    /// the receive-side filter never sees a foreign type here.
    async fn bed() -> Option<(SqsQueue, SqsDead)> {
        let client = client().await?;
        let tag = Uuid::now_v7().simple().to_string();
        let main = create_queue(&client, &format!("nook-test-{tag}")).await;
        let dlq = create_queue(&client, &format!("nook-test-{tag}-dlq")).await;
        let q = SqsQueue::new(client.clone(), main, Some(dlq.clone()));
        let dead = SqsDead {
            client,
            dlq_url: dlq,
        };
        Some((q, dead))
    }

    macro_rules! skip_or {
        () => {{
            let Some(pair) = bed().await else {
                eprintln!("skipping — no reachable test SQS (NOOK_TEST_SQS_ENDPOINT)");
                return;
            };
            pair
        }};
    }

    /// Reads the DLQ for the contract's dead-letter assertions. Uses a zero
    /// visibility so repeated scans see the same messages, and dedupes by id.
    struct SqsDead {
        client: Client,
        dlq_url: String,
    }

    impl SqsDead {
        async fn scan(&self) -> Vec<(Uuid, String, String)> {
            let mut seen = std::collections::HashMap::new();
            for _ in 0..3 {
                let resp = self
                    .client
                    .receive_message()
                    .queue_url(&self.dlq_url)
                    .max_number_of_messages(10)
                    .visibility_timeout(0)
                    .wait_time_seconds(1)
                    .message_attribute_names("All")
                    .send()
                    .await
                    .expect("dlq scan");
                for m in resp.messages() {
                    let id = str_of(m, ATTR_ID)
                        .and_then(|s| Uuid::parse_str(&s).ok())
                        .unwrap_or(Uuid::nil());
                    let ty = str_of(m, ATTR_TYPE).unwrap_or_default();
                    let reason = str_of(m, ATTR_REASON).unwrap_or_default();
                    seen.insert(id, (ty, reason));
                }
            }
            seen.into_iter().map(|(id, (ty, r))| (id, ty, r)).collect()
        }
    }

    #[async_trait::async_trait]
    impl DeadInspect for SqsDead {
        async fn dead_count(&self, work_type: &str) -> i64 {
            self.scan()
                .await
                .iter()
                .filter(|(_, ty, _)| ty == work_type)
                .count() as i64
        }
        async fn dead_reason(&self, id: Uuid) -> Option<String> {
            self.scan()
                .await
                .into_iter()
                .find(|(mid, _, _)| *mid == id)
                .map(|(_, _, r)| r)
        }
    }

    #[tokio::test]
    async fn enqueue_receive_ack_round_trip() {
        let (q, _d) = skip_or!();
        contract::enqueue_receive_ack_round_trip(&q).await;
    }
    #[tokio::test]
    async fn visibility_expiry_redelivers() {
        let (q, _d) = skip_or!();
        // SQS visibility timeout is integer-seconds, and `receive` long-polls up
        // to a second, so pass 2s — comfortably above both — rather than the
        // 300ms the in-memory/redis backends use.
        contract::visibility_expiry_redelivers(&q, Duration::from_secs(2)).await;
    }
    #[tokio::test]
    async fn nack_requeue_and_dead() {
        let (q, d) = skip_or!();
        contract::nack_requeue_and_dead(&q, &d).await;
    }
    #[tokio::test]
    async fn max_attempts_exhaustion_dead_letters() {
        let (q, d) = skip_or!();
        contract::max_attempts_exhaustion_dead_letters(&q, &d).await;
    }
    #[tokio::test]
    async fn receive_filters_by_type() {
        let (q, _d) = skip_or!();
        contract::receive_filters_by_type(&q).await;
    }
    #[tokio::test]
    async fn concurrent_receivers_never_double_deliver() {
        let (q, _d) = skip_or!();
        contract::concurrent_receivers_never_double_deliver(&q).await;
    }
}
