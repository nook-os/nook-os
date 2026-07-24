//! Notifications: raise once, deliver everywhere.
//!
//! The shape is deliberately one-way. Something happens → a `Notification` row
//! is written → it is pushed to every connected client AND handed to every
//! channel whose filters match. Nothing calls Slack directly; nothing needs to
//! know which channels exist.
//!
//! ## Why a trait rather than a match
//!
//! Adding Discord should be one file and one row in [`KINDS`], not an edit to
//! every call site. [`Channel`] is what a provider has to implement, and
//! everything else here — filtering, fan-out, recording success and failure —
//! is written once and shared.
//!
//! ## Why delivery is detached
//!
//! A channel is somebody else's HTTP server. It can be slow, down, or a black
//! hole, and the thing that raised the notification is usually in the middle of
//! a request somebody is waiting on. So the row is written synchronously — it
//! is the durable part, and the inbox must never lose it — and delivery is
//! spawned. A failing Slack webhook makes a task move slower by exactly zero.

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use nook_types::*;
use serde_json::Value;
use sha2::Sha256;
use sqlx::PgPool;

use crate::state::AppState;

/// Sign an outbound webhook body.
///
/// `t=<unix>,v1=<hex hmac of "t.body">` — the timestamp is inside the signed
/// material, so a receiver that checks it can reject a replay. Without it a
/// captured request stays valid forever.
///
/// The scheme is deliberately the one Stripe and GitHub use: a receiver that
/// has verified either already has code that does this, and a bespoke scheme
/// would be one more thing to get wrong on the side that cannot be tested from
/// here.
pub fn sign(secret: &str, body: &str, unix_ts: i64) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length");
    mac.update(format!("{unix_ts}.{body}").as_bytes());
    format!("t={unix_ts},v1={}", hex(&mac.finalize().into_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Refuse URLs that point back inside the network NookOS is running in.
///
/// A channel URL is attacker-chosen in the sense that matters: anyone who can
/// configure one can make the control plane issue an authenticated-looking
/// request from inside the perimeter. That is server-side request forgery, and
/// the classic targets are the cloud metadata endpoint and services that
/// listen only on loopback because they assumed nothing outside could reach
/// them.
///
/// Checked at CONFIGURATION time, where the person can be told, and again at
/// DELIVERY, because DNS can change between the two.
pub fn guard_url(url: &str) -> anyhow::Result<()> {
    let parsed = url::Url::parse(url).map_err(|e| anyhow::anyhow!("not a URL: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => anyhow::bail!("{other}:// is not allowed — use http or https"),
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("that URL has no host"))?;

    // Resolve, then check every address it answers with: a name that resolves
    // to one public and one private address is still a way in.
    let addrs: Vec<std::net::IpAddr> = match host.parse::<std::net::IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => {
            use std::net::ToSocketAddrs;
            let port = parsed.port_or_known_default().unwrap_or(443);
            (host, port)
                .to_socket_addrs()
                .map_err(|e| anyhow::anyhow!("cannot resolve {host}: {e}"))?
                .map(|s| s.ip())
                .collect()
        }
    };
    if addrs.is_empty() {
        anyhow::bail!("{host} does not resolve");
    }
    for ip in addrs {
        if is_internal(ip) {
            anyhow::bail!(
                "{host} resolves to {ip}, which is inside this network — a notification channel must point somewhere external"
            );
        }
    }
    Ok(())
}

fn is_internal(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()   // 169.254.x — cloud metadata lives here
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                // 100.64/10 carrier-grade NAT, which Tailscale also uses.
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
                // 0.0.0.0/8
                || v4.octets()[0] == 0
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // fc00::/7 unique-local and fe80::/10 link-local.
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // ::ffff:127.0.0.1 is loopback wearing a hat, and would
                // otherwise sail past every check above.
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_internal(std::net::IpAddr::V4(v4)))
        }
    }
}

/// One outbound integration.
///
/// Kept tiny on purpose: everything a provider does not have an opinion about
/// — when to fire, who to tell, what counts as an error worth showing — is the
/// dispatcher's job, so a new provider is a `deliver` and nothing else.
#[async_trait]
pub trait Channel: Send + Sync {
    /// Matches `notification_channels.kind`.
    fn id(&self) -> &'static str;

    /// What the UI needs to render a form for this provider.
    fn describe(&self) -> ChannelKind;

    /// Send it. Returning `Err` records the message against the channel so a
    /// quietly-broken integration is visible rather than merely silent.
    ///
    /// `mailer` is the control plane's shared [`Mailer`](crate::mailer::Mailer):
    /// the Email channel delivers through it, so there is one mail path to
    /// configure; every other channel ignores it and is byte-for-byte unchanged.
    async fn deliver(
        &self,
        cfg: &Value,
        n: &Notification,
        mailer: &dyn crate::mailer::Mailer,
    ) -> anyhow::Result<()>;
}

/// Every provider NookOS knows how to talk to.
pub fn channels() -> Vec<Box<dyn Channel>> {
    vec![
        Box::new(Webhook),
        Box::new(Slack),
        Box::new(Discord),
        Box::new(Telegram),
        Box::new(Ntfy),
        Box::new(Twilio),
        Box::new(Email),
    ]
}

pub fn kinds() -> Vec<ChannelKind> {
    channels().iter().map(|c| c.describe()).collect()
}

fn field(name: &str, label: &str, placeholder: &str, secret: bool) -> ChannelField {
    ChannelField {
        name: name.into(),
        label: label.into(),
        placeholder: placeholder.into(),
        secret,
        required: true,
    }
}

fn http() -> reqwest::Client {
    // A channel that never answers must not hold a task open forever. Ten
    // seconds is long enough for any of these APIs and short enough that a
    // dead endpoint is noticed rather than accumulated.
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

/// Turn a non-2xx into an error that says what came back.
///
/// "delivery failed" is useless in a UI; "403 invalid_token" is actionable, and
/// the body is where every one of these providers puts the reason.
async fn ok_or_body(resp: reqwest::Response) -> anyhow::Result<()> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    anyhow::bail!(
        "{} {}",
        status.as_u16(),
        body.trim().chars().take(300).collect::<String>()
    )
}

// ── providers ───────────────────────────────────────────────────────────────

/// The generic escape hatch: POST the notification as JSON.
///
/// First in the list because it is the one that never needs NookOS to be
/// updated — anything that can receive a POST is already supported.
struct Webhook;

#[async_trait]
impl Channel for Webhook {
    fn id(&self) -> &'static str {
        "webhook"
    }
    fn describe(&self) -> ChannelKind {
        ChannelKind {
            id: "webhook".into(),
            label: "Webhook".into(),
            description: "POST the notification as JSON to any URL.".into(),
            fields: vec![field("url", "URL", "https://example.com/hooks/nook", false)],
        }
    }
    async fn deliver(
        &self,
        cfg: &Value,
        n: &Notification,
        _mailer: &dyn crate::mailer::Mailer,
    ) -> anyhow::Result<()> {
        let url = str_field(cfg, "url")?;
        // Re-checked here, not only when it was configured: DNS can change
        // between the two, which is the whole point of a rebinding attack.
        guard_url(url)?;

        // Signed over the EXACT bytes sent, so the receiver verifies what it
        // parsed rather than a re-serialisation of it — key order and float
        // formatting are not guaranteed to survive a round trip.
        let body = serde_json::to_string(n)?;
        let ts = chrono::Utc::now().timestamp();
        let mut req = http()
            .post(url)
            .header("content-type", "application/json")
            .header("user-agent", "NookOS")
            // Named so a receiver can find the docs, and versioned inside the
            // value (`v1=`) so the scheme can change without a new header.
            .header("x-nook-event", n.kind.clone())
            .header("x-nook-delivery", n.id.to_string());
        if let Some(secret) = cfg.get("__secret").and_then(Value::as_str) {
            req = req.header("x-nook-signature", sign(secret, &body, ts));
        }
        ok_or_body(req.body(body).send().await?).await
    }
}

struct Slack;

#[async_trait]
impl Channel for Slack {
    fn id(&self) -> &'static str {
        "slack"
    }
    fn describe(&self) -> ChannelKind {
        ChannelKind {
            id: "slack".into(),
            label: "Slack".into(),
            description: "An incoming webhook URL from a Slack app.".into(),
            fields: vec![field(
                "webhook_url",
                "Webhook URL",
                "https://hooks.slack.com/services/…",
                true,
            )],
        }
    }
    async fn deliver(
        &self,
        cfg: &Value,
        n: &Notification,
        _mailer: &dyn crate::mailer::Mailer,
    ) -> anyhow::Result<()> {
        let url = str_field(cfg, "webhook_url")?;
        let mut text = format!("{} *{}*", emoji(&n.level), n.title);
        if !n.body.is_empty() {
            text.push_str(&format!("\n{}", n.body));
        }
        if let Some(link) = &n.link {
            text.push_str(&format!("\n<{link}|open in NookOS>"));
        }
        ok_or_body(
            http()
                .post(url)
                .json(&serde_json::json!({ "text": text }))
                .send()
                .await?,
        )
        .await
    }
}

struct Discord;

#[async_trait]
impl Channel for Discord {
    fn id(&self) -> &'static str {
        "discord"
    }
    fn describe(&self) -> ChannelKind {
        ChannelKind {
            id: "discord".into(),
            label: "Discord".into(),
            description: "A channel webhook URL from Discord.".into(),
            fields: vec![field(
                "webhook_url",
                "Webhook URL",
                "https://discord.com/api/webhooks/…",
                true,
            )],
        }
    }
    async fn deliver(
        &self,
        cfg: &Value,
        n: &Notification,
        _mailer: &dyn crate::mailer::Mailer,
    ) -> anyhow::Result<()> {
        let url = str_field(cfg, "webhook_url")?;
        let mut content = format!("{} **{}**", emoji(&n.level), n.title);
        if !n.body.is_empty() {
            content.push_str(&format!("\n{}", n.body));
        }
        if let Some(link) = &n.link {
            content.push_str(&format!("\n{link}"));
        }
        ok_or_body(
            http()
                .post(url)
                .json(&serde_json::json!({ "content": content }))
                .send()
                .await?,
        )
        .await
    }
}

struct Telegram;

#[async_trait]
impl Channel for Telegram {
    fn id(&self) -> &'static str {
        "telegram"
    }
    fn describe(&self) -> ChannelKind {
        ChannelKind {
            id: "telegram".into(),
            label: "Telegram".into(),
            description: "A bot token from @BotFather, and the chat to post in.".into(),
            fields: vec![
                field("bot_token", "Bot token", "123456:ABC-DEF…", true),
                field("chat_id", "Chat ID", "-1001234567890", false),
            ],
        }
    }
    async fn deliver(
        &self,
        cfg: &Value,
        n: &Notification,
        _mailer: &dyn crate::mailer::Mailer,
    ) -> anyhow::Result<()> {
        let token = str_field(cfg, "bot_token")?;
        let chat = str_field(cfg, "chat_id")?;
        let mut text = format!("{} {}", emoji(&n.level), n.title);
        if !n.body.is_empty() {
            text.push_str(&format!("\n{}", n.body));
        }
        if let Some(link) = &n.link {
            text.push_str(&format!("\n{link}"));
        }
        ok_or_body(
            http()
                .post(format!("https://api.telegram.org/bot{token}/sendMessage"))
                .json(&serde_json::json!({ "chat_id": chat, "text": text }))
                .send()
                .await?,
        )
        .await
    }
}

/// ntfy.sh — the simplest way to get a push notification onto a phone without
/// registering an app with anybody.
struct Ntfy;

#[async_trait]
impl Channel for Ntfy {
    fn id(&self) -> &'static str {
        "ntfy"
    }
    fn describe(&self) -> ChannelKind {
        ChannelKind {
            id: "ntfy".into(),
            label: "Push (ntfy)".into(),
            description: "Phone push via ntfy.sh or your own ntfy server.".into(),
            fields: vec![
                field("server", "Server", "https://ntfy.sh", false),
                field("topic", "Topic", "my-nook-alerts", false),
            ],
        }
    }
    async fn deliver(
        &self,
        cfg: &Value,
        n: &Notification,
        _mailer: &dyn crate::mailer::Mailer,
    ) -> anyhow::Result<()> {
        let server = str_field(cfg, "server").unwrap_or("https://ntfy.sh");
        let topic = str_field(cfg, "topic")?;
        guard_url(server)?;
        let mut req = http()
            .post(format!("{}/{}", server.trim_end_matches('/'), topic))
            .header("Title", n.title.clone())
            .header(
                "Priority",
                match n.level.as_str() {
                    "error" => "urgent",
                    "warning" => "high",
                    _ => "default",
                },
            )
            .body(n.body.clone());
        if let Some(link) = &n.link {
            req = req.header("Click", link.clone());
        }
        ok_or_body(req.send().await?).await
    }
}

struct Twilio;

#[async_trait]
impl Channel for Twilio {
    fn id(&self) -> &'static str {
        "twilio"
    }
    fn describe(&self) -> ChannelKind {
        ChannelKind {
            id: "twilio".into(),
            label: "SMS (Twilio)".into(),
            description: "Text messages. Costs money per notification — filter it.".into(),
            fields: vec![
                field("account_sid", "Account SID", "AC…", false),
                field("auth_token", "Auth token", "", true),
                field("from", "From number", "+15550001111", false),
                field("to", "To number", "+15550002222", false),
            ],
        }
    }
    async fn deliver(
        &self,
        cfg: &Value,
        n: &Notification,
        _mailer: &dyn crate::mailer::Mailer,
    ) -> anyhow::Result<()> {
        let sid = str_field(cfg, "account_sid")?;
        let token = str_field(cfg, "auth_token")?;
        let from = str_field(cfg, "from")?;
        let to = str_field(cfg, "to")?;
        // SMS is charged per segment, so the body is trimmed rather than sent
        // whole — a stack trace would cost real money to deliver.
        let mut text = n.title.clone();
        if !n.body.is_empty() {
            text.push_str(": ");
            text.push_str(&n.body);
        }
        let text: String = text.chars().take(300).collect();
        ok_or_body(
            http()
                .post(format!(
                    "https://api.twilio.com/2010-04-01/Accounts/{sid}/Messages.json"
                ))
                .basic_auth(sid, Some(token))
                .form(&[("From", from), ("To", to), ("Body", &text)])
                .send()
                .await?,
        )
        .await
    }
}

/// Email the notification through the control plane's SHARED mailer — one mail
/// path to configure, whatever `MAIL_PROVIDER` it is (capture logs it, smtp
/// sends it). The channel carries only a recipient; there is no per-channel SMTP
/// (NG-2), no secret.
///
/// Design note (built by nobody here): a future TWO-WAY integration — a
/// Slack-chat bot that also receives — will implement a SEPARATE trait alongside
/// `Channel`, never merged into it. `Channel` stays one-way delivery; email is
/// just one more impl of it (NG-1/NG-5).
struct Email;

#[async_trait]
impl Channel for Email {
    fn id(&self) -> &'static str {
        "email"
    }
    fn describe(&self) -> ChannelKind {
        ChannelKind {
            id: "email".into(),
            label: "Email".into(),
            description: "Email the notification via the server's configured mail transport."
                .into(),
            fields: vec![field("to", "To", "you@example.com", false)],
        }
    }
    async fn deliver(
        &self,
        cfg: &Value,
        n: &Notification,
        mailer: &dyn crate::mailer::Mailer,
    ) -> anyhow::Result<()> {
        let to = str_field(cfg, "to")?;
        // Subject = title; body = the notification body, with its deep link
        // appended when present (the same information ntfy puts in Click).
        let mut text = n.body.clone();
        if let Some(link) = &n.link {
            text.push_str("\n\n");
            text.push_str(link);
        }
        mailer.send(to, &n.title, &text, None).await
    }
}

fn str_field<'a>(cfg: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    cfg.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("this channel is missing its `{key}` setting"))
}

fn emoji(level: &str) -> &'static str {
    match level {
        "success" => "✅",
        "warning" => "⚠️",
        "error" => "🔴",
        _ => "•",
    }
}

// ── raising and fanning out ─────────────────────────────────────────────────

/// A notification about to be raised.
#[derive(Debug, Clone)]
pub struct Draft {
    pub level: String,
    pub title: String,
    pub body: String,
    pub kind: String,
    pub link: Option<String>,
    pub payload: Value,
    pub user_id: Option<uuid::Uuid>,
}

impl Draft {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            level: "info".into(),
            title: title.into(),
            body: String::new(),
            kind: "custom".into(),
            link: None,
            payload: Value::Null,
            user_id: None,
        }
    }
    pub fn level(mut self, l: impl Into<String>) -> Self {
        self.level = l.into();
        self
    }
    pub fn body(mut self, b: impl Into<String>) -> Self {
        self.body = b.into();
        self
    }
    pub fn kind(mut self, k: impl Into<String>) -> Self {
        self.kind = k.into();
        self
    }
    pub fn link(mut self, l: impl Into<String>) -> Self {
        self.link = Some(l.into());
        self
    }
    pub fn payload(mut self, p: Value) -> Self {
        self.payload = p;
        self
    }
}

/// Write it, push it to every connected client, and hand it to the channels.
///
/// Never returns an error: a notification that cannot be raised must not fail
/// the thing it was reporting on. That would make adding a notification to a
/// code path a way of introducing a new failure mode to it.
pub async fn raise(state: &AppState, tenant: TenantId, draft: Draft) {
    let level = match draft.level.as_str() {
        "success" | "warning" | "error" => draft.level.clone(),
        _ => "info".to_string(),
    };

    let row: Result<Notification, _> = sqlx::query_as(
        "INSERT INTO notifications (id, tenant_id, user_id, level, title, body, kind, link, payload)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         RETURNING id, tenant_id, user_id, level, title, body, kind, link, payload,
                   read_at, created_at",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(tenant)
    .bind(draft.user_id)
    .bind(&level)
    .bind(&draft.title)
    .bind(&draft.body)
    .bind(&draft.kind)
    .bind(&draft.link)
    .bind(if draft.payload.is_null() {
        serde_json::json!({})
    } else {
        draft.payload.clone()
    })
    .fetch_one(&state.db)
    .await;

    let notification = match row {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "could not record a notification");
            return;
        }
    };

    // Clients first: it is instant, local, and the thing somebody is actually
    // looking at.
    state.registry.publish(
        tenant,
        nook_proto::UiEvent::Notification {
            notification: serde_json::to_value(&notification).unwrap_or_default(),
        },
    );

    // Then everywhere else, detached — see the module note.
    let state = state.clone();
    tokio::spawn(async move {
        fan_out(&state, tenant, &notification).await;
    });
}

/// A channel as the dispatcher needs it — including `config`, which is why
/// this never leaves this module.
#[derive(sqlx::FromRow)]
struct ChannelRow {
    id: uuid::Uuid,
    kind: String,
    config: Value,
    levels: Vec<String>,
    kinds: Vec<String>,
    secret: Option<String>,
}

impl ChannelRow {
    /// `config` plus the signing secret, which lives in its own column so it
    /// is never returned by an endpoint that returns config.
    fn config_with_secret(&self) -> Value {
        let mut c = self.config.clone();
        if let (Some(obj), Some(secret)) = (c.as_object_mut(), self.secret.as_deref()) {
            obj.insert("__secret".into(), Value::String(secret.into()));
        }
        c
    }
}

async fn fan_out(state: &AppState, tenant: TenantId, n: &Notification) {
    let rows: Vec<ChannelRow> = match sqlx::query_as(
        "SELECT id, kind, config, levels, kinds, secret FROM notification_channels
         WHERE tenant_id = $1 AND enabled",
    )
    .bind(tenant)
    .fetch_all(&state.db)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "could not load notification channels");
            return;
        }
    };

    let providers = channels();
    for row in rows {
        if !matches_filters(n, &row.levels, &row.kinds) {
            continue;
        }
        let Some(provider) = providers.iter().find(|p| p.id() == row.kind) else {
            tracing::warn!(kind = %row.kind, "no provider for this channel kind");
            continue;
        };
        let result = provider
            .deliver(&row.config_with_secret(), n, &*state.mailer)
            .await;
        record_outcome(&state.db, row.id, result).await;
    }
}

/// Empty means "everything" for both filters, and `kinds` is prefix-matched so
/// `task.` catches every task event without listing them.
pub fn matches_filters(n: &Notification, levels: &[String], kinds: &[String]) -> bool {
    if !levels.is_empty() && !levels.iter().any(|l| l == &n.level) {
        return false;
    }
    if !kinds.is_empty() && !kinds.iter().any(|k| n.kind.starts_with(k.as_str())) {
        return false;
    }
    true
}

async fn record_outcome(db: &PgPool, channel: uuid::Uuid, result: anyhow::Result<()>) {
    let (ok, err) = match result {
        Ok(()) => (true, None),
        Err(e) => {
            tracing::warn!(channel = %channel, error = %e, "notification delivery failed");
            (
                false,
                Some(e.to_string().chars().take(500).collect::<String>()),
            )
        }
    };
    let _ = sqlx::query(
        "UPDATE notification_channels
         SET last_ok_at = CASE WHEN $2 THEN now() ELSE last_ok_at END,
             last_error = $3,
             updated_at = now()
         WHERE id = $1",
    )
    .bind(channel)
    .bind(ok)
    .bind(err)
    .execute(db)
    .await;
}

/// Deliver to exactly one channel, for the "test" button.
///
/// A channel you cannot test is one you find out about when the thing it was
/// supposed to tell you about has already happened.
pub async fn test_channel(
    state: &AppState,
    tenant: TenantId,
    id: uuid::Uuid,
) -> anyhow::Result<()> {
    let row: Option<ChannelRow> = sqlx::query_as(
        "SELECT id, kind, config, levels, kinds, secret FROM notification_channels
         WHERE id = $1 AND tenant_id = $2",
    )
    .bind(id)
    .bind(tenant)
    .fetch_optional(&state.db)
    .await?;
    let row = row.ok_or_else(|| anyhow::anyhow!("no such channel"))?;
    let (kind, config) = (row.kind.clone(), row.config_with_secret());
    let provider = channels()
        .into_iter()
        .find(|p| p.id() == kind)
        .ok_or_else(|| anyhow::anyhow!("no provider for {kind}"))?;

    let sample = Notification {
        id: uuid::Uuid::now_v7(),
        tenant_id: tenant,
        user_id: None,
        level: "success".into(),
        title: "NookOS test notification".into(),
        body: "If you can read this, the channel works.".into(),
        kind: "test".into(),
        link: Some(state.cfg.public_base_url.clone()),
        payload: serde_json::json!({ "test": true }),
        read_at: None,
        created_at: chrono::Utc::now(),
    };
    let result = provider.deliver(&config, &sample, &*state.mailer).await;
    let message = result.as_ref().err().map(|e| e.to_string());
    record_outcome(&state.db, id, result).await;
    match message {
        Some(m) => Err(anyhow::anyhow!(m)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(level: &str, kind: &str) -> Notification {
        Notification {
            id: uuid::Uuid::nil(),
            tenant_id: TenantId(uuid::Uuid::nil()),
            user_id: None,
            level: level.into(),
            title: "t".into(),
            body: String::new(),
            kind: kind.into(),
            link: None,
            payload: Value::Null,
            read_at: None,
            created_at: chrono::Utc::now(),
        }
    }

    /// Empty filters mean "everything". Getting this backwards would make a
    /// newly-added channel silently deliver nothing, which looks identical to
    /// a channel that is broken.
    #[test]
    fn no_filters_means_everything() {
        let n = sample("info", "task.created");
        assert!(matches_filters(&n, &[], &[]));
    }

    #[test]
    fn levels_are_exact_and_kinds_are_prefixes() {
        let n = sample("error", "task.created");
        assert!(matches_filters(&n, &["error".into()], &[]));
        assert!(!matches_filters(&n, &["info".into()], &[]));

        // A prefix so `task.` catches every task event without listing them.
        assert!(matches_filters(&n, &[], &["task.".into()]));
        assert!(matches_filters(&n, &[], &["task.created".into()]));
        assert!(!matches_filters(&n, &[], &["session.".into()]));

        // Both filters must pass, not either.
        assert!(!matches_filters(&n, &["info".into()], &["task.".into()]));
    }

    /// Every provider must be reachable by the `kind` stored on a row, or a
    /// channel saved through the UI silently delivers nothing.
    #[test]
    fn every_provider_is_addressable_and_described() {
        for c in channels() {
            let d = c.describe();
            assert_eq!(d.id, c.id(), "describe() must use the provider's own id");
            assert!(!d.label.is_empty(), "{} needs a label", c.id());
            assert!(!d.fields.is_empty(), "{} needs at least one field", c.id());
        }
        let ids: Vec<&str> = channels().iter().map(|c| c.id()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "duplicate provider ids: {ids:?}");
    }

    /// A missing setting must name the setting. "delivery failed" sends
    /// somebody to the logs; "missing `chat_id`" sends them to the form.
    #[tokio::test]
    async fn a_missing_setting_names_itself() {
        let n = sample("info", "custom");
        let mailer = crate::mailer::capture::CaptureMailer::new();
        let err = Telegram
            .deliver(&serde_json::json!({ "bot_token": "x" }), &n, &mailer)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("chat_id"), "{err}");
    }

    /// The Email channel delivers through the shared mailer: `send` is called
    /// with the configured `to`, the notification title as subject, and the body
    /// (plus its deep link) as the text — captured by the capture provider (AC-3).
    #[tokio::test]
    async fn email_channel_sends_via_the_shared_mailer() {
        let mut n = sample("warning", "task.assigned");
        n.title = "You were assigned MAIN-9".into();
        n.body = "Ready to build.".into();
        n.link = Some("https://nook.example/board?task=MAIN-9".into());

        let mailer = crate::mailer::capture::CaptureMailer::new();
        Email
            .deliver(
                &serde_json::json!({ "to": "you@example.test" }),
                &n,
                &mailer,
            )
            .await
            .expect("capture send is Ok");

        let sent = mailer.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].to, "you@example.test");
        assert_eq!(sent[0].subject, "You were assigned MAIN-9");
        assert!(sent[0].text_body.contains("Ready to build."));
        assert!(
            sent[0]
                .text_body
                .contains("https://nook.example/board?task=MAIN-9"),
            "the deep link rides in the body: {:?}",
            sent[0].text_body
        );
    }

    /// A missing `to` names the setting, like every other channel.
    #[tokio::test]
    async fn email_without_a_recipient_names_the_setting() {
        let n = sample("info", "custom");
        let mailer = crate::mailer::capture::CaptureMailer::new();
        let err = Email
            .deliver(&serde_json::json!({}), &n, &mailer)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("`to`"), "{err}");
    }

    /// The registry (and thus the UI's kind list) includes email.
    #[test]
    fn kinds_include_email_with_a_to_field() {
        let email = kinds()
            .into_iter()
            .find(|k| k.id == "email")
            .expect("email is a registered kind");
        assert_eq!(email.fields.len(), 1);
        assert_eq!(email.fields[0].name, "to");
    }
}

// ── keeping the inbox usable ────────────────────────────────────────────────

/// A per-tenant token bucket for `POST /notify`.
///
/// The endpoint is deliberately reachable by node tokens, because a machine
/// reporting that it finished is the whole point — which means a looping hook
/// or a compromised node can fill somebody's inbox with thousands of rows and,
/// worse, spend their Twilio balance doing it. The limit is the thing that
/// makes "any machine may notify" a safe sentence.
///
/// In memory rather than in Postgres: it is advisory, resets on restart
/// harmlessly, and a rate limiter that writes a row per request is a rate
/// limiter that costs more than what it protects.
#[derive(Default)]
pub struct RateLimiter {
    buckets: std::sync::Mutex<std::collections::HashMap<uuid::Uuid, Bucket>>,
}

struct Bucket {
    tokens: f64,
    last: std::time::Instant,
}

/// Generous for a person, immediately obvious for a loop.
const BURST: f64 = 30.0;
const PER_SECOND: f64 = 0.5;

impl RateLimiter {
    /// `true` if this tenant may raise one now.
    pub fn allow(&self, tenant: TenantId) -> bool {
        let now = std::time::Instant::now();
        let mut buckets = match self.buckets.lock() {
            Ok(b) => b,
            // A poisoned mutex must not become an outage: failing open on the
            // limiter is safer than failing closed on notifications.
            Err(e) => e.into_inner(),
        };
        let bucket = buckets.entry(tenant.0).or_insert(Bucket {
            tokens: BURST,
            last: now,
        });
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.last = now;
        bucket.tokens = (bucket.tokens + elapsed * PER_SECOND).min(BURST);
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod limit_tests {
    use super::*;

    /// A burst is fine; a loop is not. Both matter — the first is a person
    /// testing a channel, the second is a hook nobody noticed was recursive.
    #[test]
    fn a_burst_passes_and_a_flood_does_not() {
        let rl = RateLimiter::default();
        let t = TenantId(uuid::Uuid::now_v7());
        for i in 0..BURST as usize {
            assert!(rl.allow(t), "burst request {i} should pass");
        }
        assert!(
            !rl.allow(t),
            "the {}th should be refused",
            BURST as usize + 1
        );
    }

    /// Tenants must not share a bucket, or one noisy fleet silences another.
    #[test]
    fn tenants_are_limited_independently() {
        let rl = RateLimiter::default();
        let a = TenantId(uuid::Uuid::now_v7());
        let b = TenantId(uuid::Uuid::now_v7());
        for _ in 0..BURST as usize {
            rl.allow(a);
        }
        assert!(!rl.allow(a));
        assert!(rl.allow(b), "a different tenant is unaffected");
    }
}

#[cfg(test)]
mod security_tests {
    use super::*;

    /// The signature has to be stable and cover the timestamp, or a receiver
    /// cannot detect a replay.
    #[test]
    fn signatures_are_deterministic_and_cover_the_timestamp() {
        let a = sign("secret", r#"{"a":1}"#, 1_700_000_000);
        assert_eq!(
            a,
            sign("secret", r#"{"a":1}"#, 1_700_000_000),
            "deterministic"
        );
        assert!(a.starts_with("t=1700000000,v1="), "{a}");

        // Any of the three inputs changing must change the signature.
        assert_ne!(a, sign("secret", r#"{"a":1}"#, 1_700_000_001), "timestamp");
        assert_ne!(a, sign("secret", r#"{"a":2}"#, 1_700_000_000), "body");
        assert_ne!(a, sign("other", r#"{"a":1}"#, 1_700_000_000), "key");
    }

    /// Every one of these is a way to make the control plane fetch something
    /// on the caller's behalf from inside the network.
    #[test]
    fn internal_addresses_are_refused() {
        for url in [
            "http://127.0.0.1/hook",
            "http://localhost/hook",
            "http://169.254.169.254/latest/meta-data/", // cloud metadata
            "http://10.0.0.5/hook",
            "http://192.168.1.1/hook",
            "http://172.16.0.1/hook",
            "http://100.64.0.1/hook", // CGNAT / Tailscale
            "http://[::1]/hook",
            "http://[::ffff:127.0.0.1]/hook", // v4-mapped loopback
            "http://0.0.0.0/hook",
        ] {
            assert!(guard_url(url).is_err(), "must refuse {url}");
        }
        // And schemes that are not a webhook at all.
        for url in ["file:///etc/passwd", "gopher://x/", "not a url"] {
            assert!(guard_url(url).is_err(), "must refuse {url}");
        }
    }

    #[test]
    fn public_addresses_are_allowed() {
        assert!(guard_url("https://8.8.8.8/hook").is_ok());
        assert!(guard_url("https://example.com/hook").is_ok());
    }
}
