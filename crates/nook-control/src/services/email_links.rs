//! Keeping the email chain current as the work moves (MAIN-330 AC-2).
//!
//! The pipeline writes the link and its run itself, because it holds the row it
//! just created. A PR is different: it is opened by somebody who has never heard
//! of an email — a build run concluding, or a human submitting from the board —
//! and reaches the card through two separate paths. This is the one function
//! both call, so a chain cannot be completed on one path and left short on the
//! other.

use nook_types::*;

use crate::state::AppState;

/// Record the PR on every chain that ends at this card.
///
/// **Best effort, and deliberately so.** The caller has already recorded the PR
/// where it matters — on the card, which is what the reviewer and the board
/// read. A link that failed to pick it up is a gap in a cross-reference, and
/// failing the PR submission over it would trade something that matters for
/// something that does not. The gap is logged rather than swallowed.
///
/// A card with no chain — the overwhelming majority — updates nothing and says
/// nothing.
pub async fn record_pr(state: &AppState, tenant: TenantId, task: TaskId, pr_url: &str) {
    match state.email_links.set_pr_ref(tenant, task, pr_url).await {
        Ok(0) => {}
        Ok(n) => {
            tracing::debug!(%tenant, %task, pr = pr_url, links = n, "email chain now names its PR")
        }
        Err(e) => tracing::warn!(
            %tenant, %task, pr = pr_url, error = %e,
            "the PR is on the card but its email chain still names none"
        ),
    }
}

/// The read-only investigate run's two calls (MAIN-331): reading the message it
/// was seeded from, and reporting what it found.
///
/// **Both are addressed by the JOB, never by the link.** The run knows its own
/// id — the control plane put it in its environment — and knows nothing else,
/// so a run cannot read a message it was not seeded from and cannot write onto
/// another message's chain. That is the same confinement `record_build_outcome`
/// gets from `NOOK_JOB_ID`, for a surface where the content is somebody's
/// support mail rather than a PR URL.
///
/// The vault lives here rather than in the repository: sealing is a decision
/// about content, and the storage layer holds bytes.
pub mod investigation {
    use nook_types::*;

    use crate::error::{ApiError, ApiResult};
    use crate::services::jobs::INVESTIGATE_KIND;
    use crate::state::AppState;

    /// The run's chain, with the two refusals that make the surface read-only
    /// and confined: a job of any other kind has no business here, and a job
    /// with no chain is a 404 rather than somebody else's row.
    async fn chain(
        state: &AppState,
        tenant: TenantId,
        viewer: UserId,
        job: JobId,
    ) -> ApiResult<EmailLink> {
        let row = state
            .jobs
            .get(tenant, job)
            .await?
            .ok_or(ApiError::NotFound)?;
        if row.kind != INVESTIGATE_KIND {
            return Err(ApiError::BadRequest(
                "only an investigate run reads a support message or reports an investigation"
                    .into(),
            ));
        }
        state
            .email_links
            .by_job(tenant, viewer, job)
            .await?
            .ok_or(ApiError::NotFound)
    }

    /// The original message, decrypted (AC-4).
    ///
    /// Lossy UTF-8 rather than a refusal: a mail transport moves 7- or 8-bit
    /// text and encodes anything else, so a byte that will not decode is a
    /// malformed delivery — and an investigation of one should read what did
    /// arrive rather than be handed an error about it.
    ///
    /// Nothing here logs the plaintext, and nothing may be added that does:
    /// this function returns the only copy the control plane makes, straight to
    /// the run that asked.
    pub async fn message(
        state: &AppState,
        tenant: TenantId,
        viewer: UserId,
        job: JobId,
    ) -> ApiResult<DecryptedMessage> {
        let link = chain(state, tenant, viewer, job).await?;
        let sealed = state
            .user_content_store
            .get(&link.storage_key)
            .await
            .map_err(|e| {
                ApiError::Internal(anyhow::anyhow!(
                    "the sealed original of this chain is unreadable: {e}"
                ))
            })?;
        let raw = state.vault.decrypt(&sealed).map_err(|e| {
            ApiError::Internal(anyhow::anyhow!("unsealing the original message: {e}"))
        })?;
        Ok(DecryptedMessage {
            message: String::from_utf8_lossy(&raw).into_owned(),
        })
    }

    /// Record the findings and the sealed draft (AC-2).
    ///
    /// Both halves in one write, because they are one report: a run that
    /// managed the analysis and not the reply has not done the job, and half a
    /// report on the record reads exactly like a whole one.
    pub async fn record(
        state: &AppState,
        tenant: TenantId,
        viewer: UserId,
        job: JobId,
        report: &InvestigationReport,
    ) -> ApiResult<EmailLink> {
        let findings = report.findings.trim();
        let draft = report.draft_reply.trim();
        if findings.is_empty() || draft.is_empty() {
            return Err(ApiError::BadRequest(
                "an investigation reports both findings and a draft reply".into(),
            ));
        }
        let link = chain(state, tenant, viewer, job).await?;

        // Sealed BEFORE the write, so a vault failure leaves the row untouched
        // rather than storing findings beside a draft that never landed.
        let sealed = state
            .vault
            .encrypt(draft.as_bytes())
            .map_err(|e| ApiError::Internal(anyhow::anyhow!("sealing the draft reply: {e}")))?;

        state
            .email_links
            .set_investigation(tenant, link.id, findings, sealed)
            .await?;

        let link = state
            .email_links
            .by_job(tenant, viewer, job)
            .await?
            .ok_or(ApiError::NotFound)?;

        // The one place a reply leaves without a human, and it happens here
        // because a draft landing is the only event `auto_send` can hang off.
        // Best effort: a tenant that opted in must not lose an investigation
        // because its relay was down.
        Ok(super::reply::auto_send(state, tenant, viewer, &link).await)
    }
}

/// Delivering the drafted reply, three ways, under one per-tenant policy
/// (MAIN-332).
///
/// The epic's rule is that **no customer is emailed unless the tenant asked for
/// it** (NG-1), and this module is where that is true or not. It is true by
/// construction rather than by care: [`recipient`] is the only function in the
/// tree that reads a chain's `customer_address`, it is pure, and it hands that
/// address back only for the two policies a tenant explicitly selected. Every
/// send goes through it.
///
/// Two triggers, one send path. A human approving is [`send`]; a tenant on
/// `auto_send` gets [`auto_send`], called as the investigation lands. Both end
/// in [`deliver`], so "which address, threaded how, recorded where" has one
/// answer rather than one per door.
pub mod reply {
    use chrono::Utc;
    use nook_types::*;
    use uuid::Uuid;

    use crate::error::{ApiError, ApiResult};
    use crate::mailer::{Category, SendOutcome, Threading};
    use crate::state::AppState;

    /// The tenant-scoped setting. Absent — the shipped default — is
    /// [`Policy::ToStaffer`], which emails no customer at all.
    ///
    /// A key of its own rather than a field on `email.inbound`, because the two
    /// answer different questions and are set by different people at different
    /// times: one is "where does our support mail arrive", the other is "how
    /// much do we trust a drafted reply". Folding the second into the first
    /// would also make every routing edit a chance to change the reply policy
    /// by omission.
    pub const SETTING_KEY: &str = "email.reply_policy";

    /// How far a drafted reply travels on its own.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Policy {
        /// (a) To the support staffer who forwarded the report. The platform
        /// emails no customer in this mode, ever. The default.
        ToStaffer,
        /// (b) To the customer, once a human has approved it in-app.
        ApproveThenSend,
        /// (c) To the customer, as soon as the investigation drafts it.
        AutoSend,
    }

    impl Policy {
        /// What a tenant that has said nothing gets. The safe direction: the
        /// failure of this default is a reply a staffer has to forward on by
        /// hand, and the failure of the other one is a machine-written email to
        /// somebody's customer.
        pub const DEFAULT: Policy = Policy::ToStaffer;

        pub fn as_str(self) -> &'static str {
            match self {
                Policy::ToStaffer => "to_staffer",
                Policy::ApproveThenSend => "approve_then_send",
                Policy::AutoSend => "auto_send",
            }
        }

        /// The wire form → the policy, or `None` for anything else. Strict, so
        /// the settings write can refuse a typo by name instead of storing it
        /// and quietly behaving as `to_staffer` forever.
        pub fn parse(s: &str) -> Option<Policy> {
            match s.trim() {
                "to_staffer" => Some(Policy::ToStaffer),
                "approve_then_send" => Some(Policy::ApproveThenSend),
                "auto_send" => Some(Policy::AutoSend),
                _ => None,
            }
        }

        pub const ALL: [Policy; 3] = [Policy::ToStaffer, Policy::ApproveThenSend, Policy::AutoSend];

        /// A stored settings value → the policy in force.
        ///
        /// Fails **closed**, and the failure is the whole reason this is not
        /// `parse` on the JSON: a value written by hand, left over from an
        /// older spelling, or of the wrong type reads as the default rather
        /// than as an opt-in nobody made.
        pub fn from_value(v: Option<&serde_json::Value>) -> Policy {
            match v.and_then(serde_json::Value::as_str).map(Policy::parse) {
                Some(Some(p)) => p,
                Some(None) | None => Policy::DEFAULT,
            }
        }
    }

    /// This tenant's policy. A read error reads as the default, for the same
    /// fail-closed reason `loops::enabled` gives: a transient blip must not
    /// start emailing customers.
    pub async fn policy(state: &AppState, tenant: TenantId) -> Policy {
        Policy::from_value(
            state
                .settings
                .tenant_value(tenant, SETTING_KEY)
                .await
                .unwrap_or(None)
                .as_ref(),
        )
    }

    /// Refuse an `email.reply_policy` write naming something this build does
    /// not implement (AC-1).
    ///
    /// Hung off the generic settings endpoint beside `email.inbound`'s check,
    /// and for the sharper version of the same reason: an unrecognised value
    /// here is read as `to_staffer`, so a tenant that meant to opt in would
    /// believe they had, see nothing sent, and have no error to look at. That
    /// endpoint already requires `tenant.manage` for a tenant-scoped write,
    /// which is what makes (b)/(c) an explicit administrative opt-in.
    pub fn validate_setting(key: &str, value: &serde_json::Value) -> ApiResult<()> {
        if key != SETTING_KEY {
            return Ok(());
        }
        let ok = value.as_str().is_some_and(|s| Policy::parse(s).is_some());
        if !ok {
            return Err(ApiError::BadRequest(format!(
                "{SETTING_KEY} is one of {}",
                Policy::ALL
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        Ok(())
    }

    /// **Who a reply goes to under this policy — the only read of
    /// `customer_address` there is (AC-6).**
    ///
    /// Pure, and the entire enforcement of the epic's NG-1: a customer address
    /// can be reached only through a `Policy` value, and `to_staffer` — the
    /// default, and what an unreadable or absent setting resolves to — cannot
    /// produce one. There is deliberately no "fall back to the other address"
    /// arm: a mode with no address to send to refuses by name, because falling
    /// back is exactly how a reply meant for a staffer reaches a customer.
    pub fn recipient(policy: Policy, link: &EmailLink) -> ApiResult<String> {
        match policy {
            Policy::ToStaffer => link.staffer_address.clone().ok_or_else(|| {
                ApiError::Conflict(
                    "this chain records no forwarding staffer, so there is nobody to \
                     deliver the draft to"
                        .into(),
                )
            }),
            Policy::ApproveThenSend | Policy::AutoSend => {
                link.customer_address.clone().ok_or_else(|| {
                    ApiError::Conflict(
                        "this chain records no reply address — the forwarded message carried \
                         no Reply-To, so there is no customer address to answer"
                            .into(),
                    )
                })
            }
        }
    }

    /// Where the reply belongs in the customer's thread (AC-5).
    ///
    /// `References` is the parent's own chain followed by the parent's id,
    /// which is RFC 5322's rule; the delivery's `In-Reply-To` is as much of its
    /// chain as this record kept. A message that carried no `Message-Id` gets
    /// no threading at all rather than an invented one — an `In-Reply-To`
    /// naming an id nobody sent threads a reply onto nothing.
    pub fn threading(link: &EmailLink) -> Threading {
        Threading {
            in_reply_to: link.message_id.clone(),
            references: link
                .in_reply_to
                .iter()
                .chain(link.message_id.iter())
                .cloned()
                .collect(),
        }
    }

    /// What a delivery with no subject is answered as.
    const NO_SUBJECT: &str = "(no subject)";

    /// `Re:` the original, without stacking a second one onto a subject that is
    /// already a reply.
    pub fn subject(link: &EmailLink) -> String {
        let original = link.subject.as_deref().unwrap_or_default().trim();
        let original = if original.is_empty() {
            NO_SUBJECT
        } else {
            original
        };
        if original.to_ascii_lowercase().starts_with("re:") {
            original.to_string()
        } else {
            format!("Re: {original}")
        }
    }

    /// A human approved the draft: send it where this tenant's policy says
    /// (AC-2, AC-3).
    ///
    /// `edited` is the text a staffer changed in the inbox before approving.
    /// It replaces the sealed draft before anything is sent, so the chain
    /// records what actually went out rather than what the run first wrote.
    pub async fn send(
        state: &AppState,
        tenant: TenantId,
        viewer: UserId,
        id: Uuid,
        edited: Option<&str>,
    ) -> ApiResult<EmailLink> {
        let link = state
            .email_links
            .by_id(tenant, viewer, id)
            .await?
            .ok_or(ApiError::NotFound)?;

        if let Some(edited) = edited {
            let edited = edited.trim();
            if edited.is_empty() {
                return Err(ApiError::BadRequest(
                    "an approved reply cannot be empty".into(),
                ));
            }
            let sealed = state.vault.encrypt(edited.as_bytes()).map_err(|e| {
                ApiError::Internal(anyhow::anyhow!("sealing the approved reply: {e}"))
            })?;
            state
                .email_links
                .set_draft_reply(tenant, link.id, sealed)
                .await?;
        }

        deliver(state, tenant, viewer, &link, policy(state, tenant).await).await
    }

    /// The `auto_send` path (AC-4): a tenant that opted all the way in gets the
    /// reply sent as the investigation reports it, with no human in between.
    ///
    /// Takes the link the caller already has and gives one back either way, so
    /// the investigation's own result is never lost to a delivery problem — a
    /// failed auto-send is a loud log and a chain whose reply is still
    /// unsent, which the inbox shows and a human can approve by hand.
    pub async fn auto_send(
        state: &AppState,
        tenant: TenantId,
        viewer: UserId,
        link: &EmailLink,
    ) -> EmailLink {
        // A re-reported investigation — a repair pass over the same card — is a
        // second reading of one message, not a second message. The chain has
        // already answered, and saying so here is what keeps the log below
        // meaning "something went wrong".
        if policy(state, tenant).await != Policy::AutoSend || link.reply_sent_at.is_some() {
            return link.clone();
        }
        match deliver(state, tenant, viewer, link, Policy::AutoSend).await {
            Ok(sent) => sent,
            Err(e) => {
                tracing::error!(
                    %tenant, link = %link.id, error = %e,
                    "this tenant is on auto_send but the drafted reply did not go out — it is \
                     on the chain, unsent, for a human to approve"
                );
                link.clone()
            }
        }
    }

    /// Claim, send, and hand the chain back — the one delivery path both
    /// triggers end in.
    ///
    /// **The claim is taken BEFORE the transport is asked**, and released only
    /// when the transport refuses. Read-then-send would let two approves a
    /// second apart both pass and put the same reply in a customer's inbox
    /// twice; this way the second is refused by name. A crash between the claim
    /// and the send therefore leaves the chain reading "sent" for a message
    /// whose fate nobody knows — which is the direction to fail in when the
    /// recipient is somebody's customer.
    async fn deliver(
        state: &AppState,
        tenant: TenantId,
        viewer: UserId,
        link: &EmailLink,
        policy: Policy,
    ) -> ApiResult<EmailLink> {
        if let Some(sent) = link.reply_sent_at {
            return Err(ApiError::Conflict(format!(
                "this chain's reply already went to {} at {}",
                link.reply_recipient.as_deref().unwrap_or("its recipient"),
                sent.to_rfc3339(),
            )));
        }

        let recipient = recipient(policy, link)?;
        let sealed = state
            .email_links
            .draft_reply_enc(tenant, link.id)
            .await?
            .ok_or_else(|| {
                ApiError::Conflict(
                    "this chain has no drafted reply yet — its investigation has not \
                     reported one"
                        .into(),
                )
            })?;
        let body = state
            .vault
            .decrypt_string(&sealed)
            .map_err(|e| ApiError::Internal(anyhow::anyhow!("unsealing the draft reply: {e}")))?;

        if state
            .email_links
            .claim_reply(tenant, link.id, &recipient, Utc::now())
            .await?
            == 0
        {
            return Err(ApiError::Conflict(
                "this chain's reply is already being sent".into(),
            ));
        }

        let sent = state
            .mailer
            .send_threaded(
                &recipient,
                &subject(link),
                &body,
                None,
                Category::Transactional,
                &threading(link),
            )
            .await;

        let held = match sent {
            Ok(SendOutcome::Delivered) => None,
            Ok(SendOutcome::Held(why)) => {
                Some(ApiError::Conflict(format!("the reply was not sent: {why}")))
            }
            Err(e) => Some(ApiError::Internal(anyhow::anyhow!(
                "sending the reply: {e}"
            ))),
        };
        if let Some(err) = held {
            state.email_links.release_reply(tenant, link.id).await?;
            return Err(err);
        }

        tracing::info!(
            %tenant, link = %link.id, policy = policy.as_str(),
            "a drafted support reply was sent"
        );
        state
            .email_links
            .by_id(tenant, viewer, link.id)
            .await?
            .ok_or(ApiError::NotFound)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;

        fn link() -> EmailLink {
            EmailLink {
                id: Uuid::nil(),
                workspace_id: None,
                task_id: TaskId(Uuid::nil()),
                loop_job_id: None,
                pr_ref: None,
                message_id: Some("<m1@acme.example>".into()),
                in_reply_to: Some("<m0@example.net>".into()),
                staffer_address: Some("staffer@acme.example".into()),
                customer_address: Some("customer@example.net".into()),
                subject: Some("the login page 500s".into()),
                reply_sent_at: None,
                reply_recipient: None,
                storage_key: "k".into(),
                findings: None,
                has_draft_reply: false,
                created_at: Utc::now(),
            }
        }

        /// The whole of NG-1, on the one function that can reach a customer.
        #[test]
        fn only_an_opted_in_policy_yields_a_customer_address() {
            let link = link();
            assert_eq!(
                recipient(Policy::ToStaffer, &link).expect("the staffer"),
                "staffer@acme.example"
            );
            for opted_in in [Policy::ApproveThenSend, Policy::AutoSend] {
                assert_eq!(
                    recipient(opted_in, &link).expect("the customer"),
                    "customer@example.net"
                );
            }
        }

        /// No fall-back arm: a mode with no address refuses rather than
        /// answering the other party.
        #[test]
        fn a_missing_address_is_a_refusal_and_never_the_other_one() {
            let no_customer = EmailLink {
                customer_address: None,
                ..link()
            };
            assert!(recipient(Policy::ApproveThenSend, &no_customer).is_err());
            assert!(recipient(Policy::AutoSend, &no_customer).is_err());

            let no_staffer = EmailLink {
                staffer_address: None,
                ..link()
            };
            assert!(recipient(Policy::ToStaffer, &no_staffer).is_err());
        }

        /// Absent, malformed, or a spelling this build does not know: all read
        /// as the mode that emails no customer.
        #[test]
        fn an_unreadable_setting_is_the_safe_default() {
            assert_eq!(Policy::from_value(None), Policy::ToStaffer);
            assert_eq!(Policy::from_value(Some(&json!(null))), Policy::ToStaffer);
            assert_eq!(Policy::from_value(Some(&json!(true))), Policy::ToStaffer);
            assert_eq!(
                Policy::from_value(Some(&json!("autosend"))),
                Policy::ToStaffer
            );
            assert_eq!(
                Policy::from_value(Some(&json!("auto_send"))),
                Policy::AutoSend
            );
            assert_eq!(
                Policy::from_value(Some(&json!(" approve_then_send "))),
                Policy::ApproveThenSend
            );
        }

        /// A write is refused rather than stored and silently read as the
        /// default — the reason `from_value` alone is not enough.
        #[test]
        fn the_settings_write_refuses_a_policy_this_build_does_not_implement() {
            for bad in [
                json!("autosend"),
                json!(true),
                json!(null),
                json!(["auto_send"]),
            ] {
                assert!(
                    validate_setting(SETTING_KEY, &bad).is_err(),
                    "accepted {bad}"
                );
            }
            for p in Policy::ALL {
                validate_setting(SETTING_KEY, &json!(p.as_str())).expect("a known policy");
            }
            // Every other key passes straight through.
            validate_setting("loops.enabled", &json!(true)).expect("not this key's business");
        }

        #[test]
        fn a_reply_threads_onto_the_message_that_started_the_chain() {
            let t = threading(&link());
            assert_eq!(t.in_reply_to.as_deref(), Some("<m1@acme.example>"));
            assert_eq!(
                t.references,
                vec![
                    "<m0@example.net>".to_string(),
                    "<m1@acme.example>".to_string()
                ]
            );

            // No `Message-Id` on the delivery, so nothing to answer: an
            // invented id would thread the reply onto a message nobody sent.
            let orphan = EmailLink {
                message_id: None,
                in_reply_to: None,
                ..link()
            };
            assert!(threading(&orphan).is_empty());
        }

        #[test]
        fn the_subject_answers_without_stacking_a_second_re() {
            assert_eq!(subject(&link()), "Re: the login page 500s");
            let already = EmailLink {
                subject: Some("RE: the login page 500s".into()),
                ..link()
            };
            assert_eq!(subject(&already), "RE: the login page 500s");
            let none = EmailLink {
                subject: Some("  ".into()),
                ..link()
            };
            assert_eq!(subject(&none), format!("Re: {NO_SUBJECT}"));
        }
    }
}
