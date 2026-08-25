//! `nook get …` and friends: a kubectl-shaped client for the control plane.
//!
//! Authentication reuses the node token already stored in `node.toml` by
//! `nook setup` — if this machine is part of a NookOS instance, its CLI can
//! talk to that instance with no extra login.

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::config::NodeConfig;

/// The tenant a command should act in when nothing on the command line says.
///
/// `NOOK_TENANT_ID` is set INSIDE a workspace session, by whoever started it,
/// so an agent running there is already scoped: `nook issues create` resolves
/// one board instead of asking which. It is per-session and disappears with the
/// session, which is what makes it safe — it is not a mode anybody has to
/// remember they are in, and there is no stale global default to get wrong.
///
/// `--tenant` overrides it; both fall back to the token's home tenant.
fn ambient_tenant() -> Option<String> {
    std::env::var("NOOK_TENANT_ID")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[derive(Clone)]
pub struct Client {
    base: String,
    token: String,
    http: reqwest::Client,
    /// Which tenant to act in, when it is not the token's home one.
    ///
    /// A tenant is this system's namespace. The token's own tenant is HOME and
    /// stays the default, so every existing caller is unchanged; this is only
    /// set when something asked to be somewhere else. Sent per request and
    /// checked against membership server-side, which is why there is no list to
    /// keep in sync — being added to an org works on the next command.
    tenant: Option<String>,
}

impl Client {
    /// Build a client, preferring the operator's own credential.
    ///
    /// `nook login` writes a user token; if one is present it wins, because it
    /// can do everything this person can — including drive machines other than
    /// this one. The node token is the fallback: always there on a joined
    /// machine, but the control plane confines it to that machine.
    pub fn from_config() -> Result<Self> {
        let node = NodeConfig::load().ok();
        if let Ok(auth) = crate::config::AuthConfig::load() {
            let base = auth
                .server
                .clone()
                .or_else(|| node.as_ref().map(|c| c.server.clone()))
                .context("logged in but no server — re-run `nook login --server <url>`")?;
            return Ok(Self {
                base: base.trim_end_matches('/').to_string(),
                token: auth.token,
                http: reqwest::Client::new(),
                tenant: ambient_tenant(),
            });
        }
        let cfg = node.context(
            "not connected — run `nook login` with a user token, or `nook setup` to join this machine",
        )?;
        let base = cfg.server.trim_end_matches('/').to_string();
        // Same rule as the agent connection: a CLI call carries the same
        // credential, so it gets the same refusal.
        let insecure = crate::config::check_server_security(&base, false)?;
        crate::config::warn_if_insecure(insecure, &base);
        Ok(Self {
            base,
            token: cfg.node_token,
            http: reqwest::Client::new(),
            tenant: ambient_tenant(),
        })
    }

    /// Build a client that speaks as THIS MACHINE, never as the person at the
    /// keyboard (MAIN-367).
    ///
    /// [`Self::from_config`] prefers a `nook login` user token whenever one
    /// exists, which is the right default for everything a human drives. It is
    /// wrong for the git-key fetch: that route is machine-only by the owner's
    /// ruling, so a shim built with `from_config` would present a user token on
    /// any machine somebody had logged into — which is every dev box and, per
    /// the dev stack's own setup, the operator node — and be refused.
    ///
    /// Deliberately does NOT fall back to the user token when there is no node
    /// config. Falling back would send a credential this endpoint refuses and
    /// surface as a puzzling 403; "this machine has not joined a fleet" is the
    /// truthful answer, and the shim's caller degrades to plain ssh on it.
    pub fn as_this_node() -> Result<Self> {
        let cfg = NodeConfig::load()
            .context("this machine has not joined a fleet — run `nook setup` to join it")?;
        let base = cfg.server.trim_end_matches('/').to_string();
        let insecure = crate::config::check_server_security(&base, false)?;
        crate::config::warn_if_insecure(insecure, &base);
        Ok(Self {
            base,
            token: cfg.node_token,
            http: reqwest::Client::new(),
            tenant: ambient_tenant(),
        })
    }

    /// Is this client acting as a person rather than as this machine? Drives
    /// the "which node can I target" logic in `start`.
    pub fn is_user(&self) -> bool {
        self.token.starts_with("nook_user_")
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let mut req = self
            .http
            .request(method, &url)
            .bearer_auth(&self.token)
            .header("accept", "application/json");
        if let Some(t) = &self.tenant {
            req = req.header("x-nook-tenant", t);
        }
        // Which of this machine's jobs the call belongs to. Not a credential —
        // the token is — but under a NODE token it is what carries the tenant:
        // a session placed cross-tenant belongs to its workspace's org, not to
        // the machine's, and the control plane checks the claim against its own
        // records before honouring it. Harmless and ignored under a user token.
        if let Ok(sid) = std::env::var("NOOK_SESSION_ID") {
            if !sid.trim().is_empty() {
                req = req.header("x-nook-session", sid.trim());
            }
        }
        if let Some(json) = body {
            req = req.json(&json);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("could not reach {}", self.base))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                bail!(
                    "unauthorized — this node's token was rejected by {}",
                    self.base
                );
            }
            bail!("{} {}: {}", status.as_u16(), path, text.trim());
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        self.send(reqwest::Method::GET, path, None).await
    }

    /// GET where "not there" is an answer rather than a failure: `Ok(None)` on
    /// a 404, an error on anything else.
    ///
    /// The distinction is what turns another tenant's attachment id into one
    /// clean sentence instead of `404 /api/v1/attachments/…: {"error":…}`
    /// (MAIN-534 AC-4). Every other status still fails, so an outage is never
    /// reported as an absence.
    pub async fn get_opt(&self, path: &str) -> Result<Option<Value>> {
        let url = format!("{}{path}", self.base);
        let mut req = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .header("accept", "application/json");
        if let Some(t) = &self.tenant {
            req = req.header("x-nook-tenant", t);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("could not reach {}", self.base))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("{} {}: {}", status.as_u16(), path, text.trim());
        }
        Ok(Some(serde_json::from_str(&text).unwrap_or(Value::Null)))
    }

    /// GET the raw bytes of a body that is a file, not a document.
    ///
    /// Buffered rather than streamed: the store's own cap is 25 MiB, so the
    /// worst case fits in memory comfortably, and a partially written file
    /// left behind by a mid-stream failure would be worse than a slightly
    /// larger allocation.
    pub async fn get_bytes(&self, path: &str) -> Result<Vec<u8>> {
        let url = format!("{}{path}", self.base);
        let mut req = self.http.get(&url).bearer_auth(&self.token);
        if let Some(t) = &self.tenant {
            req = req.header("x-nook-tenant", t);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("could not reach {}", self.base))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("{} {}", status.as_u16(), path);
        }
        Ok(resp.bytes().await?.to_vec())
    }

    /// The same client, acting in a different tenant of the caller's.
    ///
    /// A copy rather than a mutation: `Client` is handed around by value, and a
    /// command that re-scoped the one it was given would re-scope every later
    /// call in the same process.
    pub fn in_tenant(&self, tenant: &str) -> Self {
        Client {
            tenant: Some(tenant.to_string()),
            ..self.clone()
        }
    }

    /// Stream a body straight to a file the caller has already opened.
    ///
    /// Not [`Self::get_bytes`]: a tenant export is unbounded — every attachment
    /// the tenant ever uploaded is in it — so holding it in memory to write it
    /// out would undo the whole point of the endpoint streaming (MAIN-659).
    /// Returns how many bytes were written.
    pub async fn stream_to(&self, path: &str, out: &mut std::fs::File) -> Result<u64> {
        use std::io::Write;

        let url = format!("{}{path}", self.base);
        let mut req = self.http.get(&url).bearer_auth(&self.token);
        if let Some(t) = &self.tenant {
            req = req.header("x-nook-tenant", t);
        }
        let mut resp = req
            .send()
            .await
            .with_context(|| format!("could not reach {}", self.base))?;
        let status = resp.status();
        if !status.is_success() {
            // The server's own sentence, when it sent one — "exporting a tenant
            // needs the owner role" is the only account of a 403 that helps.
            let text = resp.text().await.unwrap_or_default();
            let detail = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or_else(|| text.trim().to_string());
            bail!("{} {}: {}", status.as_u16(), path, detail);
        }
        let mut written = 0u64;
        while let Some(chunk) = resp
            .chunk()
            .await
            .context("the download ended early — the archive is incomplete")?
        {
            out.write_all(&chunk)?;
            written += chunk.len() as u64;
        }
        out.flush()?;
        Ok(written)
    }

    /// GET returning the RAW body, for endpoints whose answer is not JSON.
    ///
    /// The git-ssh shim's key material is one of those: a PEM block is not a
    /// JSON document, and wrapping it in one would only mean the shim had to
    /// unwrap it again (MAIN-367). An empty body — the 204 a workspace with no
    /// pinned credential returns — comes back as an empty string.
    pub async fn get_text(&self, path: &str) -> Result<String> {
        let url = format!("{}{}", self.base.trim_end_matches('/'), path);
        let res = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .with_context(|| format!("cannot reach {url}"))?;
        if !res.status().is_success() {
            anyhow::bail!("{} returned {}", path, res.status());
        }
        Ok(res.text().await.unwrap_or_default())
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }

    /// POST a file as `multipart/form-data` — the one shape the upload route
    /// accepts (MAIN-594).
    ///
    /// The body is built here rather than through reqwest's `multipart`
    /// feature, which this workspace does not enable: the whole payload is a
    /// filename, a content type and bytes already in memory, so composing it
    /// costs a `Vec` and leaves the encoding — the part the server's parser
    /// actually reads — a pure function a test can assert on.
    ///
    /// Failures surface the SERVER's sentence when it sent one. The upload cap
    /// is configured server-side, so *"that file is larger than the 25 MiB
    /// upload limit"* is the only account of it that can be right; a client
    /// that guessed would be wrong on any deployment that retuned it.
    pub async fn post_file(
        &self,
        path: &str,
        filename: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> Result<Value> {
        let boundary = format!("nook{}", uuid::Uuid::now_v7().simple());
        let body = multipart_body(&boundary, filename, content_type, bytes);
        let url = format!("{}{path}", self.base);
        let mut req = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .header("accept", "application/json")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body);
        if let Some(t) = &self.tenant {
            req = req.header("x-nook-tenant", t);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("could not reach {}", self.base))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("{}", api_message(status.as_u16(), path, &text));
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    pub async fn delete(&self, path: &str) -> Result<Value> {
        self.send(reqwest::Method::DELETE, path, None).await
    }

    /// PUT is the idempotent-write verb the board uses for labels: "make this
    /// true", safe to repeat, which is what a retrying agent needs.
    pub async fn put(&self, path: &str, body: Value) -> Result<Value> {
        self.send(reqwest::Method::PUT, path, Some(body)).await
    }

    /// PATCH is the partial-write verb: the body names only what it wants
    /// changed and an absent key leaves that setting alone, which is a
    /// different statement from PUT's "replace this with what I sent".
    pub async fn patch(&self, path: &str, body: Value) -> Result<Value> {
        self.send(reqwest::Method::PATCH, path, Some(body)).await
    }

    /// PATCH, returning the HTTP status alongside the body so a caller can react
    /// to a 409 (optimistic-concurrency conflict) instead of only an error
    /// string — the read-guard-retry the safe body edit needs (MAIN-36).
    pub async fn patch_status(&self, path: &str, body: Value) -> Result<(u16, Value)> {
        let url = format!("{}{path}", self.base);
        let resp = self
            .http
            .patch(&url)
            .bearer_auth(&self.token)
            .header("accept", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("could not reach {}", self.base))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        Ok((
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        ))
    }
}

/// Every workspace the caller can see, walked off the paged collection.
///
/// `GET /api/v1/workspaces` answers `{rows, next_cursor}` and is bounded
/// (MAIN-606), so a CLI that means "all of them" — a table, `--json`, a name
/// lookup — has to follow the cursor. Taking the first page instead is how
/// `nook start <repo>` would report a workspace that exists as missing.
pub async fn workspaces_all(client: &Client) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let path = match &after {
            None => "/api/v1/workspaces?limit=200".to_string(),
            Some(c) => format!("/api/v1/workspaces?limit=200&after={c}"),
        };
        let page = client.get(&path).await?;
        out.extend(
            page.get("rows")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        );
        match page.get("next_cursor").and_then(Value::as_str) {
            Some(c) => after = Some(c.to_string()),
            None => return Ok(out),
        }
    }
}

/// One `multipart/form-data` part carrying a file, encoded exactly as the
/// upload route's parser reads it.
///
/// **The `filename` is mandatory, not decorative.** `POST /user-content` skips
/// every field without one — that is how it lets a form post a caption
/// alongside its file — so a part sent without it uploads nothing and the
/// request fails as "no file in the upload".
///
/// A quote or a newline in the name would end the header early, so both are
/// replaced rather than escaped: the name is a label the server stores, and no
/// caller has ever needed one to contain a `"`.
fn multipart_body(boundary: &str, filename: &str, content_type: &str, bytes: Vec<u8>) -> Vec<u8> {
    let name: String = filename
        .chars()
        .map(|c| {
            if c == '"' || c == '\r' || c == '\n' {
                '_'
            } else {
                c
            }
        })
        .collect();
    let mut body = format!(
        "--{boundary}\r\n\
         Content-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\
         Content-Type: {content_type}\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(&bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// What to print when a request failed: the server's own sentence when it sent
/// one, the status and body when it did not.
///
/// A refusal these routes make is written for the person reading it — the
/// upload cap, the "only the person who attached this" rule — and re-wording it
/// here would mean maintaining a second copy of a policy that lives on the
/// server. `{"error": …}` is the one body shape both services emit.
fn api_message(status: u16, path: &str, text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| format!("{status} {path}: {}", text.trim()))
}

/// `nook login --token nook_user_…` — act as yourself, not as this machine.
///
/// Verifies the token before writing it, because a credential that silently
/// doesn't work is worse than one that obviously doesn't.
/// `nook login` with no token: sign in through the identity provider.
///
/// The whole point is that nobody copies a credential by hand — and that this
/// works on a machine with no web UI in front of it, which the paste-a-token
/// path quietly assumed.
pub async fn login_with_provider(server: Option<&str>) -> Result<()> {
    let base = server
        .map(str::to_string)
        .or_else(|| NodeConfig::load().ok().map(|c| c.server))
        .context("no --server given and this machine hasn't joined a control plane")?;
    let base = base.trim_end_matches('/').to_string();

    let token = crate::device_login::login(&base).await?;
    login(&token, Some(&base)).await
}

pub async fn login(token: &str, server: Option<&str>) -> Result<()> {
    if !token.starts_with("nook_user_") {
        bail!(
            "that isn't a user token — create one in Settings → Access tokens \
             (they start with nook_user_)"
        );
    }
    let base = server
        .map(str::to_string)
        .or_else(|| NodeConfig::load().ok().map(|c| c.server))
        .context("no --server given and this machine hasn't joined a control plane")?;
    let base = base.trim_end_matches('/').to_string();

    let probe = Client {
        base: base.clone(),
        token: token.to_string(),
        http: reqwest::Client::new(),
        tenant: ambient_tenant(),
    };
    let me = probe
        .get("/api/v1/auth/me")
        .await
        .context("that token was rejected")?;

    crate::config::AuthConfig {
        server: Some(base.clone()),
        token: token.to_string(),
    }
    .save()?;

    let who = me
        .get("user")
        .and_then(|u| u.get("email"))
        .and_then(Value::as_str)
        .unwrap_or("you");
    println!("✓ logged in to {base} as {who}");
    println!("  This CLI can now drive any machine in your fleet.");
    Ok(())
}

/// `nook whoami` — which credential is this CLI using, and for whom?
pub async fn whoami(tenant: Option<&str>) -> Result<()> {
    let mut client = Client::from_config()?;
    if let Some(t) = tenant {
        client.set_tenant(Some(t.to_string()));
    }
    let me = client.get("/api/v1/auth/me").await?;
    let field = |a: &str, b: &str| {
        me.get(a)
            .and_then(|o| o.get(b))
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string()
    };
    println!("server:    {}", client.base);
    println!(
        "as:        {} ({})",
        field("user", "email"),
        if client.is_user() {
            "user token — can drive any node"
        } else {
            "node token — confined to this machine"
        }
    );
    // WHERE the tenant came from, not just which one it is. A bare slug cannot
    // be told apart from the token's home, and "why is this the wrong tenant"
    // is the question this command exists to answer.
    //
    // ASKED-FOR is not the same as GOT, and saying "from NOOK_TENANT_ID" when
    // the server ignored the header is the lie this command exists to prevent:
    // the control plane drops the tenant header for a NODE token, deliberately
    // and silently, so a session reads its node's tenant while the variable
    // says otherwise. Compare the two and report the divergence.
    let asked = tenant
        .map(|t| (t.to_string(), "--tenant"))
        .or_else(|| ambient_tenant().map(|t| (t, "NOOK_TENANT_ID")));
    let slug = field("tenant", "slug");
    let id = field("tenant", "id");
    match &asked {
        Some((want, from))
            if want.eq_ignore_ascii_case(&slug) || want.eq_ignore_ascii_case(&id) =>
        {
            println!("tenant:    {slug} (from {from})");
        }
        Some((want, from)) => {
            println!("tenant:    {slug} — NOT the {want} asked for via {from}");
            if !client.is_user() {
                println!(
                    "           A node token is confined to its own machine and is never \
                     re-scoped,\n           so the tenant was ignored. Sign in as a person: \
                     `nook login --token nook_user_…`"
                );
            } else {
                println!("           The control plane did not honour it — check membership.");
            }
        }
        None => println!("tenant:    {slug} (home — the token's own)"),
    }

    // Where this shell is confined, if anywhere. `whoami` is the command people
    // run WHEN CONFUSED, so an unreadable session has to be explained here
    // rather than raised as an error the way it is everywhere else.
    let Some(sid) = std::env::var("NOOK_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        println!("session:   not in a nook session — commands act across the tenant");
        return Ok(());
    };
    match client.get(&format!("/api/v1/sessions/{sid}")).await {
        Ok(s) => {
            let name = s.get("name").and_then(Value::as_str).unwrap_or("?");
            let runtime = s.get("runtime").and_then(Value::as_str).unwrap_or("?");
            let status = s.get("status").and_then(Value::as_str).unwrap_or("?");
            println!("session:   {name} ({runtime}, {status})");
            match s.get("workspace_id").and_then(Value::as_str) {
                Some(ws) => {
                    let name = client
                        .get(&format!("/api/v1/workspaces/{ws}"))
                        .await
                        .ok()
                        .and_then(|w| w.get("name").and_then(Value::as_str).map(str::to_string))
                        .unwrap_or_else(|| ws.to_string());
                    println!("workspace: {name} — commands scope to this repo");
                }
                None => println!("workspace: none (ad-hoc terminal) — no repo confinement"),
            }
        }
        // The exact failure that silently unconfined the CLI. Naming the tenant
        // it looked in is the whole diagnosis: the session is almost always
        // real and simply lives somewhere this token was not asked to look.
        Err(_) => {
            println!(
                "session:   {sid} — NOT READABLE in tenant {}",
                field("tenant", "slug")
            );
            println!(
                "           If it belongs to another tenant, set NOOK_TENANT_ID or pass -T.\n\
                 \x20          Until then this shell has no workspace confinement."
            );
        }
    }
    Ok(())
}

/// `nook logout` — forget the user token. The node token (if any) still works
/// for this machine.
pub fn logout() -> Result<()> {
    let path = crate::config::auth_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => println!("✓ logged out ({} removed)", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("not logged in"),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

// ── Driving sessions from a script ──────────────────────────────────────────
//
// `start` → `send` → `read` is the whole vocabulary, and it is deliberately
// the same vocabulary whether the runtime on the far end is claude, hermes or
// bash, and whether the machine is this one or one three networks away. No
// ssh, no tmux, no knowing which host anything lives on: the control plane
// already knows, so the CLI asks it.

/// Pick exactly one row by name or id, or refuse and say why.
///
/// AMBIGUITY IS AN ERROR, not a coin flip. Every managed session in a workspace
/// is called "bash (managed)", so "find the first row whose name matches" acted
/// on whichever one the list happened to return first — and for `delete` that
/// is a destructive guess. An id (or any unambiguous prefix of one) identifies
/// exactly one; a name that does not is reported with the candidates so the
/// caller can pick.
///
/// `keys` is the fields worth matching for this resource. The id is matched by
/// PREFIX, like a short git sha; everything else must match in full, because a
/// prefix match on names would make `bash` ambiguous with every `bash session`.
pub fn pick_one(rows: Vec<Value>, want: &str, keys: &[&str], resource: &str) -> Result<Value> {
    let hit = |r: &Value| {
        keys.iter().any(|k| {
            r.get(*k).and_then(Value::as_str).is_some_and(|v| match *k {
                "id" => v.len() >= want.len() && v[..want.len()].eq_ignore_ascii_case(want),
                _ => v.eq_ignore_ascii_case(want),
            })
        })
    };
    let matches: Vec<Value> = rows.into_iter().filter(|r| hit(r)).collect();
    match matches.len() {
        0 => bail!("no {resource} matching '{want}' — try `nook get {resource}`"),
        1 => Ok(matches.into_iter().next().expect("checked")),
        n => {
            let mut msg = format!("'{want}' matches {n} {resource} — name one by id:\n");
            for r in matches.iter().take(10) {
                let id = r.get("id").and_then(Value::as_str).unwrap_or("?");
                let name = r
                    .get("name")
                    .or_else(|| r.get("title"))
                    .and_then(Value::as_str)
                    .unwrap_or("-");
                let status = r.get("status").and_then(Value::as_str).unwrap_or("-");
                // FULL id here, never the short form. If two rows were ever
                // truncated to the same thing, a list that repeated the
                // truncation would be a dead end — this is the one place that
                // has to be able to tell them apart.
                msg.push_str(&format!("  {id}  {name}  {status}\n"));
            }
            if n > 10 {
                msg.push_str(&format!("  … and {} more\n", n - 10));
            }
            bail!("{}", msg.trim_end())
        }
    }
}

impl Client {
    /// Point this client at a tenant for the rest of its life.
    pub fn set_tenant(&mut self, tenant: Option<String>) {
        self.tenant = tenant;
    }
}

/// Find one session by name or id. Names are what people (and agents) can
/// remember; ids are what survives a rename — and what disambiguates the many
/// sessions sharing a generated name.
async fn find_session(client: &Client, want: &str) -> Result<Value> {
    let list = client.get("/api/v1/sessions").await?;
    let rows = list.as_array().cloned().unwrap_or_default();
    pick_one(rows, want, &["name", "id"], "sessions")
}

/// `nook start <workspace> [--node] [--runtime]` — open a session anywhere in
/// the fleet and print how to talk to it.
pub async fn start(
    workspace: &str,
    node: Option<&str>,
    runtime: &str,
    name: Option<&str>,
) -> Result<()> {
    let client = Client::from_config()?;
    let ws = workspaces_all(&client)
        .await?
        .into_iter()
        .find(|w| {
            ["name", "slug", "id"]
                .iter()
                .filter_map(|k| w.get(*k).and_then(Value::as_str))
                .any(|v| v.eq_ignore_ascii_case(workspace))
        })
        .with_context(|| format!("no workspace named '{workspace}' — try `nook get workspaces`"))?;

    // A workspace can be checked out on several machines; a session has to
    // name one. Prefer the requested node, then any online checkout.
    //
    // The exception is a node token: the control plane confines it to its own
    // machine, so when that's all we have, a local checkout is preferred over a
    // remote one — it turns a guaranteed 403 into the thing the caller meant.
    // Logged in as a person, that preference would be wrong: "any online node"
    // means any, and the fleet is the point.
    let locations = ws
        .get("locations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let self_node_id = NodeConfig::load().ok().map(|c| c.node_id);
    let online = |l: &&Value| l.get("node_status").and_then(Value::as_str) == Some("online");
    let named = |l: &&Value| {
        node.is_none_or(|n| {
            l.get("node_name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                == n
        })
    };
    let prefer_local = !client.is_user();
    let location = locations
        .iter()
        .filter(online)
        .filter(named)
        .find(|l| {
            prefer_local
                && self_node_id
                    .as_deref()
                    .is_some_and(|id| l.get("node_id").and_then(Value::as_str) == Some(id))
        })
        .or_else(|| locations.iter().filter(online).find(named))
        .with_context(|| match node {
            Some(n) => format!("'{n}' has no online checkout of this workspace"),
            None => "no online node has this workspace checked out".to_string(),
        })?;

    let body = serde_json::json!({
        "workspace_id": ws.get("id"),
        "node_id": location.get("node_id"),
        "runtime": runtime,
        "name": name,
        "path": location.get("path"),
    });
    let session = client.post("/api/v1/sessions", body).await.map_err(|e| {
        // The control plane confines a node token to its own machine. Say so
        // in the terms the person typed, not as a bare 403.
        if e.to_string().contains("own machine") {
            anyhow::anyhow!(
                "that checkout is on another machine. Run this from that node, \
                 or start the session from the web UI."
            )
        } else {
            e
        }
    })?;
    let sname = session
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("session");
    let node = location
        .get("node_name")
        .and_then(Value::as_str)
        .unwrap_or("?");
    println!(
        "{}",
        crate::style::success(&format!(
            "{} — {} on {}",
            crate::style::bold(sname),
            crate::style::accent(runtime),
            crate::style::accent(node)
        ))
    );
    println!(
        "{}",
        crate::style::hint(&format!("nook exec {sname} 'your prompt'"))
    );
    println!("{}", crate::style::hint(&format!("nook read {sname}")));
    Ok(())
}

/// `nook send <session> <text>` — type into a session from anywhere.
pub async fn send(session: &str, text: &str, enter: bool) -> Result<()> {
    let client = Client::from_config()?;
    let found = find_session(&client, session).await?;
    let id = found.get("id").and_then(Value::as_str).context("no id")?;
    client
        .post(
            &format!("/api/v1/sessions/{id}/input"),
            serde_json::json!({ "text": text, "enter": enter }),
        )
        .await?;
    println!("✓ sent to {session}");
    Ok(())
}

/// Capture a session's screen. Returns (runtime, status, text) so callers can
/// tell what they're looking at before they act on it.
async fn capture(client: &Client, id: &str, lines: u32) -> Result<(String, String, String)> {
    let out = client
        .post(
            &format!("/api/v1/sessions/{id}/output"),
            serde_json::json!({ "history_lines": lines }),
        )
        .await?;
    let field = |k: &str| {
        out.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Ok((field("runtime"), field("status"), field("text")))
}

/// `nook read <session>` — what is that shell showing right now?
pub async fn read(session: &str, lines: u32, quiet: bool) -> Result<()> {
    let client = Client::from_config()?;
    let found = find_session(&client, session).await?;
    let id = found.get("id").and_then(Value::as_str).context("no id")?;
    let (runtime, status, text) = capture(&client, id, lines).await?;
    if !quiet {
        // The header is the point: an agent reading this knows whether it is
        // talking to a claude shell or a bash prompt before it types.
        println!("── {session} · runtime={runtime} · status={status} ──");
    }
    println!("{text}");
    Ok(())
}

/// `nook exec <session> <text>` — send, wait for the runtime to stop typing,
/// print what it said.
///
/// The wait is quiescence-based rather than a fixed sleep: agents answer in
/// wildly different times, and polling until the screen stops changing is the
/// only honest way to know a reply has landed.
pub async fn exec(session: &str, text: &str, timeout_secs: u64, lines: u32) -> Result<()> {
    let client = Client::from_config()?;
    let found = find_session(&client, session).await?;
    let id = found.get("id").and_then(Value::as_str).context("no id")?;

    client
        .post(
            &format!("/api/v1/sessions/{id}/input"),
            serde_json::json!({ "text": text, "enter": true }),
        )
        .await?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut previous = String::new();
    let mut stable = 0;
    let mut last = (String::new(), String::new(), String::new());
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let snap = capture(&client, id, lines).await?;
        if snap.2 == previous {
            stable += 1;
            // Two identical reads, four seconds apart — it has stopped.
            if stable >= 2 {
                last = snap;
                break;
            }
        } else {
            stable = 0;
            previous = snap.2.clone();
        }
        last = snap;
    }
    // Echo the prompt, then the reply. The old form dumped raw scrollback
    // under a `──` header, which meant the answer arrived buried in whatever
    // else happened to be on the runtime's screen — the prompt included, twice.
    println!("{}", crate::style::prompt_echo(text));
    let body = last.2.trim();
    let mut lines = body.lines();
    if let Some(first) = lines.next() {
        println!("{}", crate::style::reply(first));
        for l in lines {
            println!("  {l}");
        }
    }
    println!(
        "{}",
        crate::style::dim(&format!("  {} · {}", last.0, last.1))
    );
    Ok(())
}

/// Resources `nook get` understands, with their singular aliases.
fn resolve_resource(kind: &str) -> Result<&'static str> {
    Ok(match kind.trim_end_matches('s') {
        "node" => "nodes",
        "session" => "sessions",
        "workspace" | "repo" => "workspaces",
        "secret" => "secrets",
        "task" => "tasks",
        "event" | "activity" => "events",
        "theme" => "themes",
        "tenant" | "namespace" => "tenants",
        other => bail!(
            "unknown resource '{other}' — try: nodes, sessions, workspaces, \
             secrets, tasks, events, themes, tenants"
        ),
    })
}

/// `nook get <resource>` — a table by default, raw JSON with --json.
pub async fn get(
    kind: &str,
    name: Option<&str>,
    json: bool,
    tenant: Option<&str>,
    all_tenants: bool,
) -> Result<()> {
    let resource = resolve_resource(kind)?;
    let mut client = Client::from_config()?;
    // `--tenant` beats `NOOK_TENANT_ID` beats the token's home. One precedence,
    // the same one kubectl uses for `-n`.
    if let Some(t) = tenant {
        client.set_tenant(Some(t.to_string()));
    }

    // `-A` is a client-side fan-out over the tenants you are a member of. The
    // server stays one-tenant-per-request — no endpoint learns to span, and no
    // credential widens — and the CLI does the joining, which is also why the
    // TENANT column can be added here rather than invented in every response.
    // Tenants are the one resource `-A` cannot span: they ARE the span. Answer
    // before the fan-out, which would otherwise ask each tenant to list the set
    // it belongs to and print it once per membership.
    if resource == "tenants" {
        return get_tenants(&client, json).await;
    }

    if all_tenants {
        return get_all_tenants(&client, resource, name, json).await;
    }

    // Secrets live under a workspace; workspaces themselves are paged
    // (MAIN-606) and both the table and `--json` mean the whole set, so the
    // cursor is walked here rather than the first page printed as if it were
    // all of them. Everything else is still a flat collection.
    let value = match resource {
        "secrets" => secrets_across_workspaces(&client, name).await?,
        "workspaces" => Value::Array(workspaces_all(&client).await?),
        _ => client.get(&format!("/api/v1/{resource}")).await?,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    let rows = value.as_array().cloned().unwrap_or_default();
    let rows = filter_rows(rows, resource, name);

    if rows.is_empty() {
        eprintln!("No {resource} found.");
        return Ok(());
    }
    // READY needs a count the workspaces endpoint does not carry, so the join
    // happens here — one extra request for the table, none for `--json`, which
    // has already returned above with the server's own shape untouched.
    let rows = if resource == "workspaces" {
        let sessions = client
            .get("/api/v1/sessions")
            .await
            .ok()
            .and_then(|v| v.as_array().cloned());
        with_session_counts(rows, sessions)
    } else {
        rows
    };
    print_table(resource, &rows);
    Ok(())
}

/// Everything in the tenant that is a secret: the named items (MAIN-625) and
/// the password-sealed `.env` files that predate them.
///
/// Both, deliberately. A `nook get secrets` that showed only the sealed files
/// would answer "what secrets does this tenant have" by hiding most of them.
/// They are one table because a reader wants one answer; SCOPE is what tells
/// them apart, and `file` is the sealed kind — a whole file, not an item, and
/// the only one this command cannot say a thing about beyond its name.
async fn secrets_across_workspaces(client: &Client, workspace: Option<&str>) -> Result<Value> {
    let all = workspaces_all(client).await?;
    let named = |ws: &Value| match workspace {
        None => true,
        Some(want) => ["name", "slug"]
            .iter()
            .filter_map(|k| ws.get(*k).and_then(Value::as_str))
            .any(|v| v.eq_ignore_ascii_case(want)),
    };

    let mut out = Vec::new();
    // Named items first: they are the tenant's, so they lead the table rather
    // than trailing one repo's files.
    //
    // Narrowing to a repo narrows these too, rather than dropping them: a
    // `nook get secrets <repo>` that showed only the sealed files would hide
    // exactly the items the unnarrowed command promises.
    let items = client.get("/api/v1/secrets").await.unwrap_or(Value::Null);
    let mut rows = items.as_array().cloned().unwrap_or_default();
    if workspace.is_some() {
        let ids: Vec<&str> = all
            .iter()
            .filter(|ws| named(ws))
            .filter_map(|ws| ws.get("id").and_then(Value::as_str))
            .collect();
        rows.retain(|r| {
            r.get("scope_id")
                .and_then(Value::as_str)
                .is_some_and(|id| ids.contains(&id))
        });
    }
    let labels = crate::secrets::labels_for(client, &rows).await;
    out.extend(crate::secrets::display_rows(&rows, &labels));

    for ws in all {
        let (Some(id), Some(name)) = (
            ws.get("id").and_then(Value::as_str),
            ws.get("name").and_then(Value::as_str),
        ) else {
            continue;
        };
        if !named(&ws) {
            continue;
        }
        let secrets = client
            .get(&format!("/api/v1/workspaces/{id}/secrets"))
            .await
            .unwrap_or(Value::Null);
        for s in secrets.as_array().cloned().unwrap_or_default() {
            let mut row = s.clone();
            if let Some(obj) = row.as_object_mut() {
                obj.insert("scope".into(), Value::String("file".into()));
                obj.insert("target".into(), Value::String(name.to_string()));
            }
            out.push(row);
        }
    }
    Ok(Value::Array(out))
}

/// `nook get tenants` — which tenants may this token act in, and what is each
/// one called.
///
/// The missing half of `-T`. Every other command took a tenant slug and
/// nothing printed the set of valid ones, so the only way to find a slug was to
/// run `-A` on some unrelated resource and read the TENANT column — and that
/// only shows tenants that happen to own a row of that resource. An empty
/// tenant was invisible.
///
/// HOME marks the token's own tenant: the one every command uses when `-T` and
/// `NOOK_TENANT_ID` are both absent.
async fn get_tenants(client: &Client, json: bool) -> Result<()> {
    let value = client.get("/api/v1/me/tenants").await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    let rows = value.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        eprintln!("No tenants found.");
        return Ok(());
    }
    let rows: Vec<Value> = rows
        .into_iter()
        .map(|mut t| {
            if let Some(obj) = t.as_object_mut() {
                // The server already resolved which tenant this request acted
                // in, honouring `-T`/`NOOK_TENANT_ID` — so `current` is the
                // scope you actually have, not a guess reconstructed here.
                let current = obj.get("current").and_then(Value::as_bool) == Some(true);
                obj.insert(
                    "scope".into(),
                    Value::String(if current { "CURRENT" } else { "-" }.into()),
                );
            }
            t
        })
        .collect();
    print_table("tenants", &rows);
    Ok(())
}

/// `-A` — the same listing, once per tenant, merged with a TENANT column.
///
/// The column appears only when the result actually spans more than one, the
/// same badge-on-presence rule `home_tenant` uses: a column that is the same
/// value on every row is noise, and its absence is itself information.
async fn get_all_tenants(
    client: &Client,
    resource: &str,
    name: Option<&str>,
    json: bool,
) -> Result<()> {
    let tenants = client.get("/api/v1/me/tenants").await?;
    let tenants = tenants.as_array().cloned().unwrap_or_default();
    if tenants.is_empty() {
        bail!("no tenants — `nook whoami` should say who you are");
    }

    let mut all: Vec<Value> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = Default::default();
    let mut sessions: Vec<Value> = Vec::new();
    for t in &tenants {
        let Some(slug) = t.get("slug").and_then(Value::as_str) else {
            continue;
        };
        let mut scoped = client.clone();
        scoped.set_tenant(Some(slug.to_string()));
        // Workspaces are paged (MAIN-606), so `-A` walks each tenant's cursor
        // for the same reason the single-tenant path does: `as_array()` on the
        // envelope is None, and the fan-out would report every tenant empty.
        let fetched = if resource == "workspaces" {
            workspaces_all(&scoped).await
        } else {
            scoped
                .get(&format!("/api/v1/{resource}"))
                .await
                .map(|v| v.as_array().cloned().unwrap_or_default())
        };
        // One tenant failing must not lose the others: a membership can be
        // revoked between the list and the fetch, and a partial answer that
        // says so beats no answer at all.
        let rows = match fetched {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!("! {slug}: {e}");
                continue;
            }
        };
        for mut r in rows {
            // The SAME id from two tenants is one object seen twice, not two
            // objects. Cross-tenant placement makes every node visible from
            // every tenant that can use it, so a six-node fleet listed as
            // twelve rows — the same machines, twice, differing only in which
            // tenant we happened to ask.
            let id = r.get("id").and_then(Value::as_str).map(str::to_string);
            if let Some(id) = &id {
                if !seen.insert(id.clone()) {
                    continue;
                }
            }
            if let Some(o) = r.as_object_mut() {
                // `home_tenant` is the API saying "this lives somewhere else",
                // and it is the honest label for a deduped row: where the thing
                // IS, not which of our questions happened to surface it.
                // `home_tenant` carries the tenant's NAME while everything
                // else here is keyed by slug, so map it back — a column that
                // said "hein" on one row and "Engineering Team" on the next
                // would look like two different kinds of thing.
                let owner = o
                    .get("home_tenant")
                    .and_then(Value::as_str)
                    .map(|home| {
                        tenants
                            .iter()
                            .find(|t| {
                                t.get("name").and_then(Value::as_str) == Some(home)
                                    || t.get("slug").and_then(Value::as_str) == Some(home)
                            })
                            .and_then(|t| t.get("slug").and_then(Value::as_str))
                            .unwrap_or(home)
                            .to_string()
                    })
                    .unwrap_or_else(|| slug.to_string());
                o.insert("tenant".into(), Value::String(owner));
            }
            all.push(r);
        }
        // READY counts this tenant's OWN sessions. Fetched inside the loop with
        // the scoped client — asking once with the home-tenant client counted
        // nothing for every other tenant, so their workspaces all read 0/-. It
        // happened to be true while those tenants had no sessions, which is the
        // worst way for a bug like this to sit.
        if resource == "workspaces" {
            if let Ok(v) = scoped.get("/api/v1/sessions").await {
                sessions.extend(v.as_array().cloned().unwrap_or_default());
            }
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&all)?);
        return Ok(());
    }
    let all = filter_rows(all, resource, name);
    // Counted AFTER dedup and filtering, so the column appears only when the
    // rows on screen really do come from more than one tenant.
    let spanned = all
        .iter()
        .filter_map(|r| r.get("tenant").and_then(Value::as_str))
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    if all.is_empty() {
        eprintln!("No {resource} found in any tenant.");
        return Ok(());
    }
    let all = if resource == "workspaces" {
        with_session_counts(all, Some(sessions))
    } else {
        all
    };
    print_table_with(resource, &all, spanned > 1);
    Ok(())
}

/// Stamp each workspace with `ready` and `nodes` for the table.
///
/// Best-effort: if the sessions call fails the columns read `-` and the rest of
/// the table still prints. A workspace list that refuses to render because a
/// second request failed would be a worse answer than a partial one.
fn with_session_counts(rows: Vec<Value>, sessions: Option<Vec<Value>>) -> Vec<Value> {
    rows.into_iter()
        .map(|mut w| {
            let Some(obj) = w.as_object_mut() else {
                return w;
            };
            let id = obj.get("id").and_then(Value::as_str).map(str::to_string);
            let nodes = obj
                .get("locations")
                .and_then(Value::as_array)
                .map(|l| l.len())
                .unwrap_or(0);
            // Desired comes from the declared spec. No spec means nothing is
            // declared, so there is no number to be short of — `-` rather than a
            // count invented from whatever happens to be running.
            let desired = obj
                .get("session_spec")
                .and_then(|s| s.get("replicas"))
                .and_then(|r| match r {
                    Value::String(s) if s == "single" => Some(1),
                    Value::Object(o) => o.get("count").and_then(Value::as_u64).map(|c| c as usize),
                    _ => None,
                });
            let ready = match (&sessions, &id) {
                (Some(all), Some(id)) => {
                    let live = all
                        .iter()
                        .filter(|s| s.get("workspace_id").and_then(Value::as_str) == Some(id))
                        .filter(|s| {
                            matches!(
                                s.get("status").and_then(Value::as_str),
                                Some("running" | "detached")
                            )
                        })
                        .count();
                    match desired {
                        Some(d) => format!("{live}/{d}"),
                        None => format!("{live}/-"),
                    }
                }
                _ => "-".into(),
            };
            obj.insert("ready".into(), Value::String(ready));
            obj.insert("nodes".into(), Value::String(nodes.to_string()));
            w
        })
        .collect()
}

/// Columns worth showing per resource; unknown resources fall back to
/// whatever scalar fields the first row has.
fn columns(resource: &str, first: &Value) -> Vec<&'static str> {
    match resource {
        // `nook get nodes` is how you check a fleet without a browser, so it
        // answers the questions you actually have about one: is it up, what is
        // it, how big, and IS IT RUNNING WHAT I DEPLOYED. That last one lived
        // only in the capabilities blob, which the table never reached into.
        "nodes" => vec![
            "name",
            "status",
            "platform",
            "capabilities.shared_operator",
            "capabilities.agent_version",
            "capabilities.cpus",
            "capabilities.memory",
            // "why is only one thing building" is a question about this
            // machine, and it was previously answerable only by ssh-ing to it
            // and reading its unit file (MAIN-508).
            "capacity",
            // Capacity says how MANY loop jobs fit; this says whether the node
            // accepts any KIND of them (MAIN-647). A node declaring none is
            // online, uncordoned, roomy and idle, and reads on this line as a
            // machine with nothing to do rather than one that would refuse
            // everything offered to it.
            "loops",
            // "why did MY tenant's work not land on MY machine" is the third
            // (MAIN-576): a node that has withdrawn its cross-tenant consent is
            // online, uncordoned and idle, and every other column agrees.
            "cross_tenant",
            // And "why is NOTHING building on it" is the other half (MAIN-505):
            // a node draining before an agent restart is online with free
            // capacity and still takes no work, which every other column on
            // this line renders as an idle machine.
            "cordon",
            // "why is nothing running on azul" has one more answer since
            // MAIN-611: a host node that cannot confine a loop agent claims no
            // loop work at all, and every other column on this line still reads
            // as a healthy idle machine.
            "sandbox",
            // And since MAIN-618 there is one more: a node below its free-disk
            // floor claims nothing either. "Is azul out of space" was a
            // question with no answer short of a shell on azul.
            "disk",
            "capabilities.runtimes",
            "last_seen_at",
        ],
        // ID FIRST, and that is the point (kubectl's shape). Every managed
        // session in a workspace is called "bash (managed)", so a table of
        // thirty of them named the same thing could not tell you which row to
        // act on — and `nook delete` matched the FIRST name it found, which is
        // whichever one the list happened to return. The id is the only handle
        // that identifies one session, so it is the first column rather than a
        // `--json` detail. Shown short; `delete`/`send`/`read` take any
        // unambiguous prefix, exactly as `git` takes a short sha.
        "sessions" => vec!["id", "name", "runtime", "status", "age"],
        // `kubectl get deployments`'s shape, because that is what a workspace
        // IS here: a declared thing the reconciler keeps at a count. READY is
        // live sessions over desired, NODES the checkouts it can run on. The
        // remote moved to `-o wide`/`--json`: it never changes and it was the
        // widest column on the line.
        "workspaces" => vec!["name", "slug", "ready", "nodes", "age"],
        "secrets" => vec!["scope", "target", "name", "updated_at"],
        // `nook secrets list`'s own table (MAIN-625). Same four columns as the
        // fold-in above, so the two surfaces read identically — SCOPE first
        // because it is what tells you who a secret reaches.
        "secret_items" => vec!["scope", "target", "name", "updated_at"],
        "tasks" => vec!["title", "column_id", "branch", "pr_url"],
        "events" => vec!["occurred_at", "kind", "actor_type"],
        "themes" => vec!["name", "slug"],
        // SLUG leads after NAME because the slug is what `-T` takes.
        "tenants" => vec!["name", "slug", "role", "scope", "id"],
        _ => first
            .as_object()
            .map(|o| {
                o.keys()
                    .filter(|k| !k.ends_with("_id") && *k != "id")
                    .take(5)
                    // Leak is fine: this runs once, in a CLI process.
                    .map(|k| Box::leak(k.clone().into_boxed_str()) as &'static str)
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// One cell, by dotted path.
///
/// Dotted because the most useful things a node reports — its agent version,
/// its core count — live under `capabilities`, and a table that could only
/// read top-level keys could not show any of them.
fn cell(row: &Value, key: &str) -> String {
    // `age` is not a field — it is `created_at` read the way a human reads it.
    // kubectl shows AGE and never a timestamp, because "how long has this been
    // here" is the question, and an ISO-8601 string makes you do the subtraction
    // yourself on every row.
    if key == "age" {
        return row
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(age_of)
            .unwrap_or_else(|| "-".into());
    }
    // Nor is `capacity`: it is `loop_capacity` read as the pair that answers the
    // question (MAIN-508 AC-4). The number alone cannot say why it is what it
    // is, and the source is the whole difference between "this box is small"
    // and "somebody cordoned it".
    if key == "capacity" {
        return capacity_cell(row.get("loop_capacity"));
    }
    if key == "cordon" {
        return cordon_cell(row.get("cordon"));
    }
    // Nor is `loops`: it is `capabilities.loop_kinds` read as the difference
    // between a node with nothing to do and a node that would take nothing
    // (MAIN-647).
    if key == "loops" {
        return loops_cell(row.pointer("/capabilities/loop_kinds"));
    }
    // Nor is `sandbox`: it is `capabilities.sandbox` read as the one thing an
    // operator does about it (MAIN-611 AC-9). `-` is a node whose agent
    // predates the field — which the dispatcher reads as "cannot", so it is
    // not the same as an empty column elsewhere.
    if key == "sandbox" {
        return sandbox_cell(row.pointer("/capabilities/sandbox"));
    }
    // Nor is `disk`: it is the tightest of the filesystems the node samples,
    // plus whether that has taken it out of the running (MAIN-618 AC-6).
    if key == "disk" {
        return disk_cell(row.get("resources"));
    }
    let mut node = row;
    for part in key.split('.') {
        match node.get(part) {
            Some(v) => node = v,
            None => return "-".into(),
        }
    }
    render_value(key, node)
}

/// A node's loop capacity as one cell: what it is HOLDING over what it may
/// hold, where the number came from, and how much of the load is not moving.
///
/// The held half is MAIN-616's: a `waiting_on_human` job keeps its container
/// and therefore its slot, so a node can sit at `2/2` with nothing actually
/// building. `2/2 (operator, 1 waiting on human)` is the line that makes that
/// legible without a shell on the box; the paused clause is omitted entirely
/// when there is nothing paused, because a parenthetical reading `0 waiting on
/// human` on every row is noise on the ordinary case.
///
/// A response that never counted (the capacity endpoints, which answer only
/// "what number is in force") has no `held`, and renders as it did before —
/// `2 (operator)`. That is different from a node holding nothing, which counts
/// zero and renders `0/2 (operator)`.
fn capacity_cell(v: Option<&Value>) -> String {
    let effective = v.and_then(|c| c.get("effective")).and_then(Value::as_i64);
    let Some(effective) = effective else {
        return "-".into();
    };
    let source = v
        .and_then(|c| c.get("source"))
        .and_then(Value::as_str)
        .unwrap_or("?");
    let held = v.and_then(|c| c.get("held")).and_then(Value::as_i64);
    let paused = v
        .and_then(|c| c.get("held_waiting_on_human"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let number = match held {
        Some(h) => format!("{h}/{effective}"),
        None => effective.to_string(),
    };
    if paused > 0 {
        format!("{number} ({source}, {paused} waiting on human)")
    } else {
        format!("{number} ({source})")
    }
}

/// A node's sandbox capability as one narrow cell (MAIN-611 AC-9).
///
/// The DETAIL is deliberately shortened here and not dropped: an operator
/// scanning a fleet needs to see which machine is refusing work, and the full
/// sentence — which names the image to build or the daemon to start — is one
/// `--json` away.
///
/// A refusal names its REASON in a word (MAIN-643 AC-6). A bare `NO` sent
/// everyone to a shell on the box to learn which of "no release published it",
/// "the registry wants a credential" and "the network refused" it was — three
/// different actions, and the middle one is the operator's own to take.
fn sandbox_cell(v: Option<&Value>) -> String {
    let Some(c) = v.filter(|v| !v.is_null()) else {
        return "-".into();
    };
    match c.get("state").and_then(Value::as_str) {
        Some("ready") => match c.get("image").and_then(Value::as_str) {
            Some(image) => format!("yes ({image})"),
            None => "yes".into(),
        },
        Some("pulling") => "pulling".into(),
        Some("exempt") => "n/a (container)".into(),
        Some("unavailable") => {
            let reason = c
                .get("reason")
                .and_then(|r| {
                    serde_json::from_value::<nook_types::SandboxUnavailable>(r.clone()).ok()
                })
                .unwrap_or_default();
            format!("NO ({})", reason.label())
        }
        _ => "-".into(),
    }
}

/// A node's free disk as one narrow cell (MAIN-618 AC-6): the TIGHTEST of the
/// filesystems it samples, and `LOW` when that has put the node below its own
/// floor and stopped it claiming loop work.
///
/// The tightest rather than each of them, for `cordon_cell`'s reason — this
/// column has to stay narrow, and the number that decides anything is the
/// smallest one. Which filesystem, and how much of it, is one `--json` away.
/// `-` is a node whose agent predates the field, which the dispatcher reads as
/// "unknown" and never as "full".
fn disk_cell(v: Option<&Value>) -> String {
    let disks = match v.and_then(|r| r.get("disks")).and_then(Value::as_array) {
        Some(d) if !d.is_empty() => d,
        _ => return "-".into(),
    };
    let Some(free) = disks
        .iter()
        .filter_map(|d| d.get("free_bytes").and_then(Value::as_u64))
        .min()
    else {
        return "-".into();
    };
    let low = v
        .and_then(|r| r.get("disk_shortage"))
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty());
    format!(
        "{:.0}G{}",
        free as f64 / 1024.0_f64.powi(3),
        if low { " LOW" } else { "" }
    )
}

/// Which loop stages a node accepts, as one cell (MAIN-647 AC-5).
///
/// `NONE` in capitals rather than the `-` an empty array renders as everywhere
/// else on this table, and that is the entire point: `-` says "nothing to
/// report", which is what a node declaring no loop kinds looked like on every
/// other column of its row. This one says it accepts nothing.
///
/// The kinds are spelled out rather than counted, because "which" is the next
/// question — a node taking only `spec` and a node taking `build` are not
/// interchangeable, and a count cannot tell them apart.
fn loops_cell(v: Option<&Value>) -> String {
    let kinds: Vec<&str> = v
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if kinds.is_empty() {
        return "NONE".into();
    }
    kinds.join(",")
}

/// A node's cordon as one narrow cell (MAIN-505): what it is waiting for and
/// how many runs are left, with `!` when the wait is past its deadline.
///
/// The reason sentence itself is deliberately NOT here — it is a sentence, and
/// it would be the widest column on the line. `nook get nodes --json` carries
/// it whole, and so does the Nodes page.
fn cordon_cell(v: Option<&Value>) -> String {
    let Some(c) = v.filter(|v| !v.is_null()) else {
        return "-".into();
    };
    let jobs = c.get("jobs_in_flight").and_then(Value::as_u64).unwrap_or(0);
    let overdue = c.get("overdue").and_then(Value::as_bool).unwrap_or(false);
    // "installing" and "waiting on 0 jobs" are different states and only the
    // flag tells them apart — a count of zero is what BOTH would print.
    let what = if c
        .get("installing")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        "installing".to_string()
    } else {
        format!("updating {jobs} job{}", if jobs == 1 { "" } else { "s" })
    };
    format!("{what}{}", if overdue { " !" } else { "" })
}

/// An RFC-3339 timestamp as an age: `45m`, `12h`, `8d`. One unit, biggest that
/// fits, which is all this column is read for.
fn age_of(ts: &str) -> Option<String> {
    let then = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    let secs = (chrono::Utc::now() - then.with_timezone(&chrono::Utc)).num_seconds();
    if secs < 0 {
        return Some("0s".into());
    }
    Some(match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    })
}

/// The SHORTEST id a table will print. A floor, not a promise — see
/// `unique_id_len`, which widens past it whenever these rows need it.
///
/// Measured, not guessed. These are uuidv7s, and 18 characters buys the full
/// 48-bit millisecond timestamp, the version nibble (always `7`, so no entropy
/// at all) and then just THREE hex digits — twelve bits, 4096 values, of the
/// random half. Two rows written in the same millisecond collide with
/// probability about 1/4096, and a burst of ten at roughly one percent.
///
/// That is not theoretical. Across 6,918 production rows: sessions, tasks and
/// checkouts are all distinct at 18, but 6,429 events yield only 6,353 distinct
/// prefixes — 76 collisions, because events are written in bursts. 8 characters
/// collides everywhere; 12 already collides among sessions.
///
/// Truncation is safe anyway, and that is the point: `pick_one` refuses an
/// ambiguous match and lists FULL ids, so the worst case is "type more
/// characters" and never "acted on the wrong row". This constant is the floor
/// for readability; correctness comes from the guard, not the length.
pub const SHORT_ID: usize = 18;

/// The shortest prefix that tells THESE ids apart, never below [`SHORT_ID`].
///
/// git's short-sha rule. A fixed width cannot be right for every table — 18 is
/// comfortable for sessions and provably too short for events — so the table
/// widens to whatever the rows in front of it require, rather than betting that
/// the chosen number outlives the next burst of writes.
fn unique_id_len(ids: &[&str]) -> usize {
    let longest = ids.iter().map(|i| i.chars().count()).max().unwrap_or(0);
    for len in SHORT_ID..longest {
        let mut seen: std::collections::BTreeSet<&str> = Default::default();
        if ids.iter().all(|i| seen.insert(i.get(..len).unwrap_or(i))) {
            return len;
        }
    }
    longest
}

fn render_value(key: &str, v: &Value) -> String {
    match v {
        Value::Null => "-".into(),
        Value::String(s) if s.is_empty() => "-".into(),
        Value::String(s) => s.clone(),
        // Raw byte counts are unreadable at a glance and are always the widest
        // column on the line.
        Value::Number(n) if key.ends_with("memory") => n
            .as_f64()
            .filter(|b| *b > 0.0)
            .map(|b| format!("{:.0}G", b / 1024.0_f64.powi(3)))
            .unwrap_or_else(|| "-".into()),
        // A boolean capability flag reads better as a mark than as
        // `true`/`false`: the shared-operator designation (MAIN-125) shows in
        // its column only on the machines that carry it, "-" everywhere else.
        Value::Bool(b) if key.ends_with("shared_operator") => {
            if *b {
                "operator".into()
            } else {
                "-".into()
            }
        }
        Value::Array(a) if a.is_empty() => "-".into(),
        // A JSON array of runtimes reads as `["bash","zsh"]`; the quotes and
        // brackets are noise in a column that is already labelled.
        Value::Array(a) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| x.to_string())
            })
            .collect::<Vec<_>>()
            .join(","),
        v => v.to_string(),
    }
}

/// `nook nodes readiness [name]` — every prerequisite a node needs before it
/// can claim work, and the command that fixes each unmet one (MAIN-647).
///
/// Two sources, one assembly. Named, it reads the capability report the control
/// plane already stores, so a machine nobody can shell into is still diagnosable
/// (AC-2); unnamed, it probes this machine, which is the only way to see a fact
/// that has not reached a Register yet.
pub async fn node_readiness(name: Option<&str>, json: bool) -> Result<()> {
    let (label, caps) = match name {
        Some(want) => {
            let client = Client::from_config()?;
            let rows = client
                .get("/api/v1/nodes")
                .await?
                .as_array()
                .cloned()
                .unwrap_or_default();
            let row = filter_rows(rows, "nodes", Some(want))
                .into_iter()
                .next()
                .with_context(|| format!("no node named {want}"))?;
            let caps: nook_types::Capabilities =
                serde_json::from_value(row.get("capabilities").cloned().unwrap_or(Value::Null))
                    .with_context(|| format!("{want} has not reported its capabilities yet"))?;
            (want.to_string(), caps)
        }
        None => {
            let caps = crate::capabilities::detect();
            (caps.hostname.clone(), caps)
        }
    };

    let gates = crate::readiness::assess(&caps);
    let (verdict, summary) = crate::readiness::summary(&gates);
    if json {
        let out: Vec<Value> = gates
            .iter()
            .map(|g| {
                serde_json::json!({
                    "gate": g.name,
                    "verdict": verdict_word(g.verdict),
                    "detail": g.detail,
                    "remedy": g.remedy,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "node": label,
                // "would this node claim loop work" — NOT "is every gate
                // answered". A machine nothing supervises still claims work,
                // right up to the self-update that ends it.
                "ready": !crate::readiness::blocked(&gates),
                "verdict": verdict_word(verdict),
                "summary": summary,
                "gates": out,
            }))?
        );
        return Ok(());
    }

    println!("{}", crate::style::bold(&format!("Readiness: {label}")));
    let width = gates.iter().map(|g| g.name.len()).max().unwrap_or(0);
    for g in &gates {
        println!(
            "{} {:width$}  {}",
            paint(g.verdict, g.verdict.mark()),
            g.name,
            g.detail
        );
        // Indented under the gate it belongs to, rather than gathered into a
        // "next steps" block at the end: the whole point is that a line saying
        // what is wrong and the line saying what to run are one thing.
        if let Some(remedy) = &g.remedy {
            println!(
                "  {:width$}  {}",
                "",
                crate::style::dim(&format!("→ {remedy}"))
            );
        }
    }
    println!();
    println!("{}", paint(verdict, summary));
    Ok(())
}

fn verdict_word(v: crate::readiness::Verdict) -> &'static str {
    match v {
        crate::readiness::Verdict::Ok => "ok",
        crate::readiness::Verdict::Warn => "warn",
        crate::readiness::Verdict::Fail => "fail",
    }
}

fn paint(v: crate::readiness::Verdict, text: &str) -> String {
    match v {
        crate::readiness::Verdict::Ok => crate::style::ok_c(text),
        crate::readiness::Verdict::Warn => crate::style::accent(text),
        crate::readiness::Verdict::Fail => crate::style::err(text),
    }
}

/// `nook get nodes buildbox` narrows a listing to one row by name/slug/id.
/// Shared so `-A` filters the merged set exactly as a single-tenant get does.
fn filter_rows(rows: Vec<Value>, resource: &str, name: Option<&str>) -> Vec<Value> {
    match (resource, name) {
        (_, Some(want)) if resource != "secrets" => rows
            .into_iter()
            .filter(|r| {
                ["name", "slug", "id", "title"]
                    .iter()
                    .filter_map(|k| r.get(*k).and_then(Value::as_str))
                    .any(|v| v.eq_ignore_ascii_case(want))
            })
            .collect(),
        _ => rows,
    }
}

pub(crate) fn print_table(resource: &str, rows: &[Value]) {
    print_table_with(resource, rows, false)
}

/// `with_tenant` prepends the TENANT column. Only true when the result actually
/// spans more than one — a column identical on every row tells you nothing, and
/// its ABSENCE is itself the information that you are looking at one tenant.
fn print_table_with(resource: &str, rows: &[Value], with_tenant: bool) {
    let mut cols = columns(resource, &rows[0]);
    if with_tenant {
        // After NAME rather than first. kubectl leads with NAMESPACE, but its
        // pod names are unique-ish sentences; here the name is what you scan
        // for and the tenant qualifies it, so it reads better one column in.
        let at = cols.iter().position(|c| *c == "name").map_or(0, |i| i + 1);
        cols.insert(at, "tenant");
    }
    // Header names the field, not its path: `CAPABILITIES.AGENT_VERSION` is a
    // location, `AGENT_VERSION` is a column.
    let headers: Vec<String> = cols
        .iter()
        .map(|c| c.rsplit('.').next().unwrap_or(c).to_uppercase())
        .collect();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    let mut body: Vec<Vec<String>> = rows
        .iter()
        .map(|r| cols.iter().map(|c| cell(r, c)).collect())
        .collect();
    // Ids arrive whole and are shortened HERE, where every row is visible, so
    // the width can be the shortest that keeps this table's ids distinct. Doing
    // it per-cell could only ever apply a fixed guess.
    if let Some(idx) = cols.iter().position(|c| *c == "id") {
        let ids: Vec<&str> = body.iter().map(|r| r[idx].as_str()).collect();
        let len = unique_id_len(&ids);
        for row in body.iter_mut() {
            row[idx] = row[idx].chars().take(len).collect();
        }
    }

    for row in &body {
        for (i, v) in row.iter().enumerate() {
            widths[i] = widths[i].max(v.chars().count());
        }
    }

    // Pad on the PLAIN text, then colour. Colouring first would count escape
    // bytes as characters and every column after the first would drift.
    let render = |cells: &[String], paint: &dyn Fn(usize, &str) -> String| {
        let mut out = String::new();
        for (i, v) in cells.iter().enumerate() {
            out.push_str(&paint(i, v));
            if i + 1 < cells.len() {
                out.push_str(&" ".repeat(widths[i] - v.chars().count() + 2));
            }
        }
        println!("{}", out.trim_end());
    };

    render(&headers, &|_, v| crate::style::dim(v));
    for row in &body {
        render(row, &|i, v| {
            // First column names the thing; the rest is detail about it.
            if i == 0 {
                return crate::style::bold(v);
            }
            match cols[i] {
                "status" => status_colour(v),
                // Timestamps are the least interesting thing on the line and
                // the widest — recede them so the eye goes to names and state.
                c if c.ends_with("_at") => crate::style::dim(v),
                _ => v.to_string(),
            }
        });
    }
}

/// Colour a status the way the UI does: green is live, red is broken,
/// everything dormant recedes.
fn status_colour(v: &str) -> String {
    use crate::style;
    match v {
        "online" | "running" | "active" | "attached" => crate::style::ok_c(v),
        "error" | "failed" | "revoked" => style::err(v),
        "offline" | "stopped" | "exited" | "-" => crate::style::dim(v),
        other => other.to_string(),
    }
}

/// `nook import` — adopt the git repository in the current directory.
///
/// The node reports repositories under its workspace roots, so importing is
/// really "make sure this repo is somewhere the node scans, then rescan".
pub async fn import(path: Option<&str>, link: bool) -> Result<()> {
    let dir = match path {
        Some(p) => std::path::PathBuf::from(crate::config::expand_path(p)),
        None => std::env::current_dir()?,
    };
    let dir = dir.canonicalize().context("no such directory")?;
    if !dir.join(".git").exists() {
        bail!("{} is not a git repository", dir.display());
    }

    let cfg = NodeConfig::load().context("run `nook setup` first")?;
    let roots: Vec<std::path::PathBuf> = cfg
        .workspace_roots
        .iter()
        .filter_map(|r| {
            std::path::Path::new(&crate::config::expand_path(r))
                .canonicalize()
                .ok()
        })
        .collect();

    // Already somewhere the node scans: nothing to place, just rescan.
    if roots.iter().any(|r| dir.starts_with(r)) {
        return finish_import(&cfg, &dir).await;
    }

    // Otherwise adopt it where it lies. The repo's own remote decides where it
    // belongs — <root>/<org>/<repo> — so two orgs' same-named repos can't
    // collide, and a symlink keeps the working copy exactly where the user has
    // it (their editor, shell history and paths all keep working).
    let Some(root) = roots.first() else {
        bail!("this node has no workspace roots — run `nook setup`");
    };
    let remote = crate::discovery::remote_of(&dir);
    let rel = remote
        .as_deref()
        .and_then(crate::gitops::repo_path_from_url)
        .or_else(|| {
            dir.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .filter(|n| !n.is_empty())
        })
        .context("could not work out a name for this repository")?;
    let dest = root.join(&rel);

    if dest.exists() {
        let same = dest.canonicalize().ok().is_some_and(|d| d == dir);
        if same {
            return finish_import(&cfg, &dir).await;
        }
        bail!(
            "{} already exists — a different checkout of {rel} is already imported",
            dest.display()
        );
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }

    if link {
        std::os::unix::fs::symlink(&dir, &dest)
            .with_context(|| format!("cannot link {} → {}", dest.display(), dir.display()))?;
        println!("✓ Linked {} → {}", dest.display(), dir.display());
    } else {
        std::fs::rename(&dir, &dest).with_context(|| {
            format!(
                "cannot move {} → {} (different filesystem? try --link)",
                dir.display(),
                dest.display()
            )
        })?;
        println!("✓ Moved {} → {}", dir.display(), dest.display());
    }
    finish_import(&cfg, &dest).await
}

/// Tell the node to rescan so the control plane reconciles the repository.
async fn finish_import(cfg: &NodeConfig, dir: &std::path::Path) -> Result<()> {
    let client = Client::from_config()?;
    client
        .post(
            &format!("/api/v1/nodes/{}/rescan", cfg.node_id),
            serde_json::json!({}),
        )
        .await?;
    println!("✓ Imported {}", dir.display());
    println!("  It appears under Workspaces once discovery reconciles it.");
    Ok(())
}

/// `nook delete <resource> <name>` — the escape hatch for cleanup.
pub async fn delete(kind: &str, names: &[String], tenant: Option<&str>) -> Result<()> {
    let resource = resolve_resource(kind)?;
    if !matches!(resource, "sessions" | "workspaces" | "tasks") {
        bail!("delete is only supported for sessions, workspaces and tasks");
    }
    let mut client = Client::from_config()?;
    if let Some(t) = tenant {
        client.set_tenant(Some(t.to_string()));
    }
    let rows = if resource == "workspaces" {
        workspaces_all(&client).await?
    } else {
        client
            .get(&format!("/api/v1/{resource}"))
            .await?
            .as_array()
            .cloned()
            .unwrap_or_default()
    };

    // RESOLVE THEM ALL BEFORE DELETING ANY. `kubectl delete pod a b c` on a
    // typo'd name deletes nothing; the alternative — delete two, then fail on
    // the third — leaves you having to work out what survived.
    let mut targets: Vec<(String, String)> = Vec::new();
    for name in names {
        let row = pick_one(
            rows.clone(),
            name,
            &["name", "slug", "id", "title"],
            resource,
        )?;
        let id = row
            .get("id")
            .and_then(Value::as_str)
            .context("row has no id")?
            .to_string();
        let label = row
            .get("name")
            .or_else(|| row.get("title"))
            .and_then(Value::as_str)
            .unwrap_or(name)
            .to_string();
        if targets.iter().any(|(existing, _)| *existing == id) {
            continue; // named twice, e.g. by id and by name
        }
        targets.push((id, label));
    }

    let mut failed = 0usize;
    for (id, label) in &targets {
        match client.delete(&format!("/api/v1/{resource}/{id}")).await {
            Ok(_) => println!(
                "✓ Deleted {} {} ({label})",
                resource.trim_end_matches('s'),
                id.chars().take(SHORT_ID).collect::<String>()
            ),
            // Keep going, and fail at the end. One gone-already row should not
            // strand the rest of a batch half-done.
            Err(e) => {
                failed += 1;
                eprintln!("✗ {} {id}: {e}", resource.trim_end_matches('s'));
            }
        }
    }
    anyhow::ensure!(failed == 0, "{failed} of {} failed", targets.len());
    Ok(())
}

/// `nook rollout restart workspace/<slug>` — kill a workspace's sessions and
/// let the reconciler put them back.
///
/// Deliberately a KILL and not a restart-in-place: there is no restart-in-place
/// to have. A session's environment — its leased ports above all — is fixed
/// when tmux creates it, so the only way to pick up a changed declaration is a
/// new session. That is exactly `kubectl rollout restart`'s bargain too.
///
/// Only sessions the reconciler manages come back. Anything hand-started is
/// gone for good, so those are listed separately before the confirmation rather
/// than quietly swept up with the rest.
/// `4510,4700-4705` → the ports it names, sorted and deduplicated.
///
/// Ranges are accepted because a machine's foreign occupants come in blocks as
/// often as singly, and making somebody expand one by hand is how a typo gets
/// into a list nothing else will ever re-read.
fn parse_port_list(spec: &str) -> Result<Vec<i32>> {
    let mut out = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let a: i32 = a
                    .trim()
                    .parse()
                    .with_context(|| format!("'{a}' is not a port number"))?;
                let b: i32 = b
                    .trim()
                    .parse()
                    .with_context(|| format!("'{b}' is not a port number"))?;
                if b < a {
                    bail!("'{part}' ends before it starts");
                }
                out.extend(a..=b);
            }
            None => out.push(
                part.parse()
                    .with_context(|| format!("'{part}' is not a port number"))?,
            ),
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// `nook set ports node/<name> <start>-<end>` — the range a node may lease from.
///
/// The setting existed and only the UI could reach it, which made the one
/// failure it causes unfixable from a terminal: a workspace declaring a
/// REQUIRED listener cannot start a session on a node with no range, and the
/// refusal names the port rather than the node's configuration.
///
/// `--clear` is a real operation, not an undo — a node that should lease
/// nothing is a legitimate state, and the endpoint spells it "neither bound".
pub async fn set_ports(
    target: &str,
    range: Option<&str>,
    clear: bool,
    exclude: Option<&str>,
    exclude_clear: bool,
    tenant: Option<&str>,
) -> Result<()> {
    // `node/azul` and `azul` both work, the same leniency `rollout` has.
    let want = target
        .split_once('/')
        .map(|(kind, rest)| {
            if matches!(kind, "node" | "nodes") {
                rest
            } else {
                target
            }
        })
        .unwrap_or(target);

    // Exclusions go to their OWN endpoint: the range body reads "neither start
    // nor end" as CLEAR THE RANGE, so posting only exclusions there would
    // silently unset it.
    let exclusions: Option<Vec<i32>> = match (exclude, exclude_clear) {
        (Some(list), _) => Some(parse_port_list(list)?),
        (None, true) => Some(Vec::new()),
        (None, false) => None,
    };
    let touching_range = range.is_some() || clear;
    if !touching_range && exclusions.is_none() {
        bail!("give a range like 4200-4299, --clear, --exclude <ports>, or --exclude-clear");
    }

    let body = match (range, clear) {
        (Some(r), _) => {
            // Parsed HERE rather than posted as text, so a typo is a message
            // about the range instead of a 400 about a field.
            let (s, e) = r.split_once('-').with_context(|| {
                format!("'{r}' is not a range — write it as <start>-<end>, e.g. 4200-4299")
            })?;
            let start: u32 = s
                .trim()
                .parse()
                .with_context(|| format!("'{}' is not a port number", s.trim()))?;
            let end: u32 = e
                .trim()
                .parse()
                .with_context(|| format!("'{}' is not a port number", e.trim()))?;
            serde_json::json!({ "start": start, "end": end })
        }
        (None, true) => serde_json::json!({}),
        // Not an error any more: this call may be exclusions-only, and the
        // range must then be left exactly as it is.
        (None, false) => serde_json::Value::Null,
    };

    let mut client = Client::from_config()?;
    if let Some(t) = tenant {
        client.set_tenant(Some(t.to_string()));
    }
    let nodes = client.get("/api/v1/nodes").await?;
    let node = pick_one(
        nodes.as_array().cloned().unwrap_or_default(),
        want,
        &["name", "hostname", "id"],
        "nodes",
    )?;
    let id = node
        .get("id")
        .and_then(Value::as_str)
        .context("node row has no id")?;
    let name = node.get("name").and_then(Value::as_str).unwrap_or(want);

    let mut got = client.get(&format!("/api/v1/nodes/{id}/ports")).await?;
    if !body.is_null() {
        got = client
            .put(&format!("/api/v1/nodes/{id}/ports"), body)
            .await?;
    }
    if let Some(ports) = exclusions {
        got = client
            .put(
                &format!("/api/v1/nodes/{id}/ports/exclusions"),
                serde_json::json!({ "ports": ports }),
            )
            .await?;
    }

    // `NodePorts`, not a bare range: the endpoint answers with the range IN
    // FORCE plus where it came from, because an operator override and the
    // node's own advertisement are different things that print the same two
    // numbers. Reading it flat silently reported every success as "no range".
    let range = got.get("range");
    let start = range.and_then(|r| r.get("start")).and_then(Value::as_i64);
    let end = range.and_then(|r| r.get("end")).and_then(Value::as_i64);
    let source = got
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match (start, end) {
        (Some(s), Some(e)) => {
            println!("✓ {name} leases {s}-{e} ({} ports, {source})", e - s + 1);
            // Concurrency, not port count, is the number an operator is
            // actually choosing — see the note on `SetCmd::Ports`.
            if let Some(a) = got.get("advertised").and_then(|a| a.get("start")) {
                if got.get("source").and_then(Value::as_str) == Some("override") {
                    println!("  the node itself advertises from {a}; this override wins");
                }
            }
        }
        // Says what the state MEANS, because "no range" reads like a failure
        // and is the thing that refuses required listeners.
        _ => println!("✓ {name} advertises no port range — it will lease nothing"),
    }
    let excluded: Vec<i64> = got
        .get("excluded")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    if !excluded.is_empty() {
        // The USABLE count, not the excluded count: capacity is what an
        // operator is choosing, and a workspace needs one port per declared
        // listener all at once.
        let inside = match (start, end) {
            (Some(s), Some(e)) => excluded.iter().filter(|p| **p >= s && **p <= e).count(),
            _ => 0,
        };
        print!(
            "  excluded: {}",
            excluded
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        match (start, end) {
            (Some(s), Some(e)) => println!(
                " — {} of {} ports usable",
                e - s + 1 - inside as i64,
                e - s + 1
            ),
            _ => println!(),
        }
    }
    Ok(())
}

/// `nook set capacity node/<name> <jobs>` — how many loop jobs a node runs at
/// once (MAIN-508).
///
/// `nook set ports`'s twin, and deliberately the same shape: the two are one
/// sizing decision about a machine, and a second style for the second half is
/// how an operator ends up unable to guess either.
///
/// Nothing restarts. The control plane reads the stored number at its next
/// dispatch poll, so raising capacity no longer costs whatever that box is
/// building.
pub async fn set_capacity(
    target: &str,
    jobs: Option<i64>,
    clear: bool,
    tenant: Option<&str>,
) -> Result<()> {
    let want = target
        .split_once('/')
        .map(|(kind, rest)| {
            if matches!(kind, "node" | "nodes") {
                rest
            } else {
                target
            }
        })
        .unwrap_or(target);

    // `--clear` is a real operation and not an undo: handing the decision back
    // to the machine is a state an operator chooses, so it is spelled rather
    // than reached by typing the node's own number back in.
    let body = match (jobs, clear) {
        (Some(n), _) => serde_json::json!({ "max_loop_jobs": n }),
        (None, true) => serde_json::json!({ "max_loop_jobs": null }),
        (None, false) => bail!("give a number of jobs (0 cordons the node), or --clear"),
    };

    let mut client = Client::from_config()?;
    if let Some(t) = tenant {
        client.set_tenant(Some(t.to_string()));
    }
    let nodes = client.get("/api/v1/nodes").await?;
    let node = pick_one(
        nodes.as_array().cloned().unwrap_or_default(),
        want,
        &["name", "hostname", "id"],
        "nodes",
    )?;
    let id = node
        .get("id")
        .and_then(Value::as_str)
        .context("node row has no id")?;
    let name = node.get("name").and_then(Value::as_str).unwrap_or(want);

    let got = client
        .put(&format!("/api/v1/nodes/{id}/capacity"), body)
        .await?;

    let effective = got.get("effective").and_then(Value::as_i64).unwrap_or(-1);
    let source = got
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    // Zero is the one value whose MEANING a bare number hides: "0" reads as a
    // failed write, and this is the per-node cordon.
    if effective == 0 {
        println!("✓ {name} runs no loop jobs ({source}) — it finishes what it holds and claims nothing new");
    } else {
        println!("✓ {name} runs {effective} loop jobs at once ({source})");
    }
    if let Some(a) = got.get("advertised").and_then(Value::as_i64) {
        if source == "operator" && a != effective {
            println!("  the node itself advertises {a}; this setting wins");
        }
    }
    println!("  in force at the next dispatch poll — nothing restarts");
    Ok(())
}

pub async fn rollout_restart(target: &str, yes: bool, tenant: Option<&str>) -> Result<()> {
    // `workspace/foo`, `workspaces/foo` or bare `foo` — the kubectl spelling
    // and the obvious one both work.
    let want = target
        .split_once('/')
        .map(|(kind, rest)| {
            if matches!(kind, "workspace" | "workspaces" | "ws") {
                rest
            } else {
                target
            }
        })
        .unwrap_or(target);

    let mut client = Client::from_config()?;
    if let Some(t) = tenant {
        client.set_tenant(Some(t.to_string()));
    }
    let ws = pick_one(
        workspaces_all(&client).await?,
        want,
        &["name", "slug", "id"],
        "workspaces",
    )?;
    let ws_id = ws.get("id").and_then(Value::as_str).context("no id")?;
    let ws_name = ws.get("name").and_then(Value::as_str).unwrap_or(want);

    let sessions = client.get("/api/v1/sessions").await?;
    let live: Vec<Value> = sessions
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.get("workspace_id").and_then(Value::as_str) == Some(ws_id))
        // A session that already exited has nothing to restart, and killing it
        // would only add noise to the output.
        .filter(|s| {
            matches!(
                s.get("status").and_then(Value::as_str),
                Some("running" | "detached" | "starting")
            )
        })
        .collect();

    if live.is_empty() {
        println!("Nothing to restart — {ws_name} has no live sessions.");
        return Ok(());
    }

    println!("Restarting {} session(s) in {ws_name}:", live.len());
    for s in &live {
        let id = s.get("id").and_then(Value::as_str).unwrap_or("?");
        let name = s.get("name").and_then(Value::as_str).unwrap_or("-");
        let status = s.get("status").and_then(Value::as_str).unwrap_or("-");
        println!(
            "  {}  {name}  {status}",
            id.chars().take(SHORT_ID).collect::<String>()
        );
    }
    println!(
        "\nThe reconciler restarts the ones it manages. Any session started by \
         hand is not replaced."
    );

    if !yes && !confirm("Kill them?")? {
        println!("Aborted — nothing was killed.");
        return Ok(());
    }

    let mut killed = 0usize;
    for s in &live {
        let Some(id) = s.get("id").and_then(Value::as_str) else {
            continue;
        };
        match client.delete(&format!("/api/v1/sessions/{id}")).await {
            Ok(_) => killed += 1,
            Err(e) => eprintln!("✗ session {id}: {e}"),
        }
    }
    println!("✓ Killed {killed} of {}. Watch them come back:", live.len());
    println!("    nook get sessions");
    Ok(())
}

/// A y/N prompt. `false` on anything that is not a clear yes, including a
/// non-interactive stdin — a rollout that ran because nobody was there to say
/// no is the failure this guards.
fn confirm(question: &str) -> Result<bool> {
    use std::io::Write;
    print!("{question} [y/N] ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return Ok(false);
    }
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes"))
}

// ── teaching the fleet ───────────────────────────────────────────────────────

/// `nook teach <file>` — one skill, every agent, every machine.
///
/// The file is read here and the control plane stores it, which is what makes
/// this different from copying a file around: nodes that are asleep right now,
/// and nodes that join next month, get it when they connect. So the summary
/// printed below distinguishes what was DELIVERED from what will converge —
/// reporting "taught 5 nodes" when two were offline would be a lie an operator
/// only discovers when an agent does not know the skill.
pub async fn teach(path: &str, name: Option<&str>) -> Result<()> {
    let content = std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?;
    anyhow::ensure!(!content.trim().is_empty(), "{path} is empty");

    // Explicit --name wins; then the document's own frontmatter; then the
    // filename. A skill named after `SKILL.md` would be called "skill" on every
    // machine in the fleet, so the filename is genuinely the last resort — and
    // when it is a bare "skill" we say so rather than shipping it.
    let derived = name.map(str::to_string).or_else(|| {
        nook_proto::skill_name_from_frontmatter(&content).or_else(|| {
            std::path::Path::new(path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_lowercase().replace(' ', "-"))
        })
    });
    let skill_name = match derived.as_deref() {
        Some("skill") | Some("skill-md") => anyhow::bail!(
            "this file has no frontmatter `name:`, so the only name left is the \
             filename — which would teach your whole fleet a skill called \
             \"skill\". Pass --name, or add a `name:` to the document."
        ),
        Some(n) => nook_proto::valid_skill_name(n).map_err(|e| anyhow::anyhow!(e))?,
        None => anyhow::bail!("cannot tell what this skill is called — pass --name"),
    };

    let client = Client::from_config()?;
    let resp = client
        .post(
            "/api/v1/skills",
            serde_json::json!({ "name": skill_name, "content": content }),
        )
        .await?;

    let delivered: Vec<String> = resp["delivered_to"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let offline: Vec<String> = resp["offline"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    println!();
    println!(
        "{} taught {} ({} bytes)",
        crate::style::ok_c("✓"),
        crate::style::bold(skill_name),
        content.len()
    );
    if !delivered.is_empty() {
        println!("  delivered to: {}", delivered.join(", "));
    }
    if !offline.is_empty() {
        // Named, not counted. "2 offline" is not something anyone can act on,
        // and it matters that this is not a failure: the skill is stored, so
        // these machines learn it the moment they reconnect.
        println!(
            "  {} {} — will learn it on reconnect",
            crate::style::dim("offline:"),
            offline.join(", ")
        );
    }
    if delivered.is_empty() && offline.is_empty() {
        println!(
            "  {}",
            crate::style::dim("no nodes have joined this control plane yet")
        );
    }
    println!();
    println!(
        "{}",
        crate::style::dim(
            "Each node writes it into every agent it finds (Hermes, Claude Code, …)."
        )
    );
    println!(
        "{}",
        crate::style::dim("See what landed where: nook get events")
    );
    Ok(())
}

/// `nook skills list` against the control plane — what the fleet has been taught.
pub async fn taught(json: bool) -> Result<()> {
    let client = Client::from_config()?;
    let resp = client.get("/api/v1/skills").await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
        return Ok(());
    }
    let rows = resp.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("Nothing taught yet. Teach your fleet a skill:");
        println!();
        println!("    nook teach ./SKILL.md");
        return Ok(());
    }
    println!("{:<24} {:>8}  UPDATED", "NAME", "SIZE");
    for r in rows {
        println!(
            "{:<24} {:>8}  {}",
            r["name"].as_str().unwrap_or("?"),
            r["size"].as_i64().unwrap_or(0),
            r["updated_at"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

/// `nook unteach <name>` — remove it here and from every machine.
pub async fn unteach(name: &str) -> Result<()> {
    let client = Client::from_config()?;
    let resp = client.delete(&format!("/api/v1/skills/{name}")).await?;
    let offline = resp["offline"].as_array().map(Vec::len).unwrap_or(0);
    println!(
        "{} removed {}",
        crate::style::ok_c("✓"),
        crate::style::bold(name)
    );
    if offline > 0 {
        println!(
            "  {}",
            crate::style::dim(&format!(
                "{offline} node(s) offline — they drop it when they reconnect"
            ))
        );
    }
    Ok(())
}

// ── the board ───────────────────────────────────────────────────────────────

/// `nook issues list` — the pick query from a terminal.
///
/// The same filter an agent uses, so a human can see exactly what the loop
/// will take next rather than inferring it from a board.
/// The workspace of the nook session this command is running inside, if any.
///
/// The whole confinement scheme rests on this: a `nook` invocation inside a
/// session reads `NOOK_SESSION_ID` (exported at session start), asks the control
/// plane which workspace that session is in, and scopes to it. `None` means the
/// var is unset (not in a nook session) or the session is ad-hoc (no
/// workspace) — in which case there is nothing to confine to and callers fall
/// back to acting across the whole tenant.
pub struct SessionWorkspace {
    /// Workspace uuid, as the API's `workspace=` filter and `workspace_id` want.
    pub id: String,
    pub name: String,
}

/// What this shell is confined to.
///
/// The three states are kept apart on purpose. "Not in a session" and "in a
/// session I could not read" both used to collapse to `None`, and `None` is the
/// PERMISSIVE answer — it means "confine to nothing". So an unreadable session
/// silently widened the caller's scope to the whole tenant.
pub enum SessionScope {
    /// No `NOOK_SESSION_ID`: a plain terminal, confined to nothing.
    Ambient,
    /// A real session that belongs to no workspace — an ad-hoc terminal.
    NoWorkspace,
    Workspace(SessionWorkspace),
}

impl SessionScope {
    /// The workspace to scope to, if there is one.
    pub fn workspace(&self) -> Option<&SessionWorkspace> {
        match self {
            SessionScope::Workspace(w) => Some(w),
            _ => None,
        }
    }
}

pub async fn current_session_scope(client: &Client) -> Result<SessionScope> {
    let Some(sid) = std::env::var("NOOK_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        return Ok(SessionScope::Ambient);
    };
    // A session id that will not resolve is NOT "no session", and must never be
    // treated as one. Unconfined is the permissive state, so failing open here
    // hands a builder the run of every repo in the tenant — which is exactly
    // what happened: a session in one tenant, a token homed in another, a 404,
    // and `nook issues list` cheerfully returned another workspace's cards.
    //
    // Cross-tenant placement makes this ORDINARY, not exotic: the workspace's
    // tenant and the node's differ routinely, and a `/sessions/{id}` read is
    // scoped to the caller's tenant, so the miss looks identical to a deleted
    // session. Refuse, and say which knob fixes it.
    let session = client
        .get(&format!("/api/v1/sessions/{sid}"))
        .await
        .with_context(|| {
            format!(
                "could not read session {sid}, which this shell says it is running in.\n\
                 If that session belongs to another tenant, name it — set NOOK_TENANT_ID \
                 or pass -T <tenant> — so the lookup happens there.\n\
                 Refusing rather than running unconfined, which would let this act on \
                 another workspace's work."
            )
        })?;
    let Some(id) = session
        .get("workspace_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return Ok(SessionScope::NoWorkspace);
    };
    // The name is for humans only; if the lookup fails, the id still confines.
    let name = client
        .get(&format!("/api/v1/workspaces/{id}"))
        .await
        .ok()
        .and_then(|w| w.get("name").and_then(|v| v.as_str()).map(str::to_string))
        .unwrap_or_else(|| "workspace".into());
    Ok(SessionScope::Workspace(SessionWorkspace { id, name }))
}

/// `nook agent-state <running|waiting|idle>` — report what the agent in this
/// session is doing, so the terminal tabs can show a spinner or a "needs you"
/// mark. Driven by the Claude Code hooks.
///
/// A no-op outside a nook session (`AC-1`): with no `NOOK_SESSION_ID` there is
/// nothing to report about, and the hook that calls this runs in plain
/// terminals too. Best-effort — a control plane that is down or slow must never
/// make an agent's turn hang or fail, so a failed report is swallowed.
pub async fn agent_state(state: &str) -> Result<()> {
    let Ok(sid) = std::env::var("NOOK_SESSION_ID") else {
        return Ok(());
    };
    if sid.is_empty() {
        return Ok(());
    }
    // The tmux window the agent is in, so the right in-session terminal chip
    // lights up rather than the whole strip. Absent when not under tmux.
    //
    // MAIN-108 exception: this runs INSIDE a session pane and must inherit the
    // pane's `$TMUX` — it deliberately gets NO `-L`. Adding the node's socket
    // here would point it at the wrong server (or none) and lose the window.
    let window = std::process::Command::new("tmux")
        .args(["display-message", "-p", "#{window_index}"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u32>().ok());

    let Ok(client) = Client::from_config() else {
        return Ok(());
    };
    let mut body = serde_json::json!({ "state": state });
    if let Some(w) = window {
        body["window"] = serde_json::json!(w);
    }
    // Best-effort: a failed report must never fail or block the agent's turn,
    // so this always returns Ok(()). But discarding the error with `let _`
    // meant a 403, a 404, or a bad NOOK_SESSION_ID looked exactly like
    // everything working — no indicator, no clue why. Leave a breadcrumb at
    // debug level so "why is my tab not spinning" is diagnosable via RUST_LOG
    // without changing the contract.
    if let Err(e) = client
        .post(&format!("/api/v1/sessions/{sid}/agent-state"), body)
        .await
    {
        tracing::debug!(error = %e, session = %sid, state, "agent-state report failed");
    }
    Ok(())
}

/// `nook workspace current` — which workspace is this session in?
///
/// The seam `/loop-spec` uses to stamp a new ticket with the workspace it was
/// filed from. Prints nothing (and exits 0) outside a workspace session, so a
/// caller can treat empty output as "unscoped" without special-casing an error.
pub async fn workspace_current(json: bool) -> Result<()> {
    let client = Client::from_config()?;
    let scope = current_session_scope(&client).await?;
    match scope.workspace() {
        Some(ws) if json => {
            println!("{}", serde_json::json!({ "id": ws.id, "name": ws.name }));
        }
        Some(ws) => println!("{}\t{}", ws.name, ws.id),
        None if json => println!("null"),
        None => {
            eprintln!("not in a workspace session (no NOOK_SESSION_ID, or an ad-hoc terminal)");
        }
    }
    Ok(())
}

/// Should a claim be refused? Pure, so the confinement policy is tested without
/// a control plane. Refuse when this session has a workspace and the task's is
/// not the same one — including a task with no workspace at all, which a
/// confined loop must not adopt. `--any-workspace` and "not in a workspace
/// session" both mean no confinement.
fn claim_blocked(session_ws: Option<&str>, task_ws: Option<&str>, any_workspace: bool) -> bool {
    if any_workspace {
        return false;
    }
    match session_ws {
        None => false,
        Some(here) => task_ws != Some(here),
    }
}

/// Resolve a `--workspace` value (a uuid or a name) to a workspace uuid.
async fn resolve_workspace(client: &Client, needle: &str) -> Result<String> {
    // A uuid is already an id; only a name needs the lookup.
    if uuid::Uuid::parse_str(needle).is_ok() {
        return Ok(needle.to_string());
    }
    workspaces_all(client)
        .await?
        .into_iter()
        .find(|w| {
            w.get("name")
                .and_then(|v| v.as_str())
                .is_some_and(|n| n.eq_ignore_ascii_case(needle))
                || w.get("slug")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s.eq_ignore_ascii_case(needle))
        })
        .and_then(|w| w.get("id").and_then(|v| v.as_str()).map(str::to_string))
        .with_context(|| format!("no workspace named '{needle}' — try `nook get workspaces`"))
}

/// The board's id: an explicit key/uuid, or the first board when none is given.
/// The create endpoint is keyed by board UUID, so a key must be resolved here.
async fn resolve_board(client: &Client, needle: Option<&str>) -> Result<String> {
    let list = client.get("/api/v1/boards").await?;
    let boards: Vec<Value> = list.as_array().cloned().unwrap_or_default();
    match needle {
        None => boards
            .first()
            .and_then(|b| b.get("id").and_then(Value::as_str).map(str::to_string))
            .context("no boards exist yet"),
        Some(n) => boards
            .iter()
            .find(|b| {
                b.get("id").and_then(Value::as_str) == Some(n)
                    || b.get("key")
                        .and_then(Value::as_str)
                        .is_some_and(|k| k.eq_ignore_ascii_case(n))
            })
            .and_then(|b| b.get("id").and_then(Value::as_str).map(str::to_string))
            .with_context(|| format!("no board '{n}' — try `nook issues list` or omit --board")),
    }
}

/// The flags for `nook issues create`, one field per flag (mirrors `main.rs`).
pub struct CreateTask {
    pub title: String,
    pub board: Option<String>,
    pub description: Option<String>,
    pub column_type: Option<String>,
    pub priority: Option<i32>,
    pub labels: Vec<String>,
    pub type_: Option<String>,
    pub parent: Option<String>,
    pub workspace: Option<String>,
}

/// The create-task request body, by the wire field names, omitting anything
/// unset so the server's own defaults apply (column → backlog, type → task,
/// visibility → team). Pure so the flag → body mapping is unit-tested.
fn build_create_body(
    opts: &CreateTask,
    description: Option<String>,
    workspace_id: Option<String>,
) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("title".into(), Value::String(opts.title.clone()));
    if let Some(d) = description {
        body.insert("description".into(), Value::String(d));
    }
    if let Some(c) = &opts.column_type {
        body.insert("column_type".into(), Value::String(c.clone()));
    }
    if let Some(p) = opts.priority {
        body.insert("priority".into(), Value::Number(p.into()));
    }
    if let Some(t) = &opts.type_ {
        body.insert("type".into(), Value::String(t.clone()));
    }
    if let Some(p) = &opts.parent {
        body.insert("parent".into(), Value::String(p.clone()));
    }
    if let Some(w) = workspace_id {
        body.insert("workspace_id".into(), Value::String(w));
    }
    if !opts.labels.is_empty() {
        body.insert(
            "labels".into(),
            Value::Array(opts.labels.iter().cloned().map(Value::String).collect()),
        );
    }
    Value::Object(body)
}

/// `nook issues create --title …` — file a task on the board (MAIN-89 AC-3).
///
/// The server owns every validation (a bad type, a non-epic parent, a blank
/// title); `Client::post` surfaces its message and this exits non-zero on a 4xx.
pub async fn create_task(opts: CreateTask) -> Result<()> {
    let client = Client::from_config()?;
    let board = resolve_board(&client, opts.board.as_deref()).await?;

    // `-` reads the body from stdin, so a multi-line spec can be piped in.
    let description = match opts.description.as_deref() {
        Some("-") => {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)
                .context("reading the description from stdin")?;
            Some(s)
        }
        other => other.map(str::to_string),
    };

    // Workspace: an explicit flag wins; otherwise inherit the session's, like
    // `nook issues list` — so a filer inside a repo files against that repo by
    // default.
    let workspace_id = match opts.workspace.as_deref() {
        Some(w) => Some(resolve_workspace(&client, w).await?),
        None => current_session_scope(&client)
            .await?
            .workspace()
            .map(|w| w.id.clone()),
    };

    let body = build_create_body(&opts, description, workspace_id);
    let created = client
        .post(&format!("/api/v1/boards/{board}/tasks"), body)
        .await?;
    let key = created["key"].as_str().unwrap_or("?");
    println!(
        "{} created {}",
        crate::style::ok_c("✓"),
        crate::style::bold(key)
    );
    if let Some(url) = created["url"].as_str() {
        println!("  {url}");
    }
    Ok(())
}

/// `nook issues relate <BLOCKER> <kind> <DEPENDENT>` (MAIN-89 AC-4).
///
/// The relation is posted on the BLOCKER. `to_task` on the endpoint is a uuid,
/// so a dependent given as a key is resolved first. After a `blocks`, the
/// dependent's blocked state is re-read so the direction is confirmed, not
/// assumed.
pub async fn relate(blocker: &str, kind: &str, dependent: &str) -> Result<()> {
    const KINDS: [&str; 3] = ["blocks", "relates", "duplicates"];
    if !KINDS.contains(&kind) {
        bail!(
            "{kind:?} is not a relation kind — expected one of {}",
            KINDS.join(", ")
        );
    }
    let client = Client::from_config()?;

    // Resolve the dependent to a uuid: the endpoint's `to_task` is a TaskId.
    let dep = client.get(&format!("/api/v1/tasks/{dependent}")).await?;
    let dep_id = dep["task"]["id"]
        .as_str()
        .with_context(|| format!("no task '{dependent}'"))?
        .to_string();

    client
        .post(
            &format!("/api/v1/tasks/{blocker}/relations"),
            serde_json::json!({ "to_task": dep_id, "kind": kind }),
        )
        .await?;
    println!(
        "{} {} {} {}",
        crate::style::ok_c("✓"),
        crate::style::bold(blocker),
        kind,
        crate::style::bold(dependent)
    );

    // Confirm direction: a `blocks` should leave the DEPENDENT blocked.
    if kind == "blocks" {
        let after = client.get(&format!("/api/v1/tasks/{dependent}")).await?;
        let blocked = after["is_blocked"].as_bool().unwrap_or(false);
        println!(
            "  {} is now {}",
            dependent,
            if blocked {
                crate::style::err("blocked")
            } else {
                "not blocked".to_string()
            }
        );
    }
    Ok(())
}

/// The `/tasks` query params, given an already-resolved workspace id. Pure so
/// the flag → query mapping (including `type`/`parent`, MAIN-89) is unit-tested
/// without a server.
#[allow(clippy::too_many_arguments)]
fn build_tasks_query(
    board: Option<&str>,
    workspace_id: Option<&str>,
    labels: &[String],
    not_labels: &[String],
    assignee: Option<&str>,
    column_type: Option<&str>,
    types: &[String],
    parent: Option<&str>,
    unblocked: bool,
    this_node: Option<&str>,
    backlog: bool,
    done: bool,
) -> Vec<String> {
    let mut q: Vec<String> = Vec::new();
    if let Some(b) = board {
        q.push(format!("board={b}"));
    }
    if let Some(w) = workspace_id {
        q.push(format!("workspace={w}"));
    }
    for l in labels {
        q.push(format!("label={l}"));
    }
    for l in not_labels {
        q.push(format!("not_label={l}"));
    }
    if let Some(a) = assignee {
        q.push(format!("assignee={a}"));
    }
    if let Some(c) = column_type {
        q.push(format!("column_type={c}"));
    }
    // Issue type (repeatable): the server ORs within types and lifts the default
    // epic exclusion when `epic` is asked for.
    for t in types {
        q.push(format!("type={t}"));
    }
    // An epic's children — the server lifts the backlog exclusion for a `parent`
    // query, so a child in Triage still shows.
    if let Some(p) = parent {
        q.push(format!("parent={p}"));
    }
    // Node affinity: the server returns cards dispatched to THIS machine plus
    // everything undispatched, so a dispatched card is a hint the loop honours
    // rather than a field nobody read.
    if let Some(n) = this_node {
        q.push(format!("node={n}"));
    }
    if unblocked {
        q.push("is_blocked=false".into());
    }
    // The backlog (and epics) are excluded server-side by default; opt in.
    if backlog {
        q.push("backlog=true".into());
    }
    // So is finished work (MAIN-464), at the other end of the board.
    if done {
        q.push("done=true".into());
    }
    q
}

// One parameter per CLI flag by design — this is the dispatch seam for
// `nook issues list`, and a struct would just move the same list one hop away.
#[allow(clippy::too_many_arguments)]
pub async fn tasks(
    board: Option<&str>,
    labels: &[String],
    not_labels: &[String],
    assignee: Option<&str>,
    column_type: Option<&str>,
    types: &[String],
    parent: Option<&str>,
    unblocked: bool,
    this_node: bool,
    workspace: Option<&str>,
    all_workspaces: bool,
    backlog: bool,
    done: bool,
    json: bool,
) -> Result<()> {
    let client = Client::from_config()?;

    // `--this-node` resolves HERE rather than server-side: the caller carries a
    // user token, so the control plane cannot tell which machine is asking. The
    // node id comes from this machine's own config; without one there is no node
    // to be, and the flag is a no-op rather than an error — a laptop that never
    // joined should still be able to run the command.
    let node_id: Option<String> = if this_node {
        match NodeConfig::load() {
            Ok(cfg) => Some(cfg.node_id),
            Err(_) => {
                eprintln!("nook: --this-node ignored — this machine has not joined a fleet");
                None
            }
        }
    } else {
        None
    };

    // Confinement. An explicit `--workspace` wins; otherwise, unless the caller
    // asked for `--all-workspaces`, a command running inside a workspace session
    // scopes to that workspace by default — so a builder agent cannot see, and
    // therefore cannot take, another repo's work just by forgetting a flag.
    let workspace_id: Option<String> = if let Some(w) = workspace {
        Some(resolve_workspace(&client, w).await?)
    } else if !all_workspaces {
        current_session_scope(&client)
            .await?
            .workspace()
            .map(|ws| ws.id.clone())
    } else {
        None
    };

    let q = build_tasks_query(
        board,
        workspace_id.as_deref(),
        labels,
        not_labels,
        assignee,
        column_type,
        types,
        parent,
        unblocked,
        node_id.as_deref(),
        backlog,
        done,
    );
    let path = if q.is_empty() {
        "/api/v1/tasks".to_string()
    } else {
        format!("/api/v1/tasks?{}", q.join("&"))
    };

    let resp = client.get(&path).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
        return Ok(());
    }
    let rows = resp.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("No tasks match.");
        return Ok(());
    }
    println!(
        "{:<10} {:<3} {:<28} {:<10} LABELS",
        "KEY", "PRI", "TITLE", "STATE"
    );
    for r in rows {
        let pri = match r["priority"].as_i64().unwrap_or(0) {
            1 => "!!",
            2 => "↑",
            3 => "=",
            4 => "↓",
            _ => "·",
        };
        let title = r["title"].as_str().unwrap_or("");
        let labels: Vec<&str> = r["labels"]
            .as_array()
            .map(|a| a.iter().filter_map(|l| l["name"].as_str()).collect())
            .unwrap_or_default();
        println!(
            "{:<10} {:<3} {:<28} {:<10} {}",
            r["key"].as_str().unwrap_or("—"),
            pri,
            if title.chars().count() > 28 {
                format!("{}…", title.chars().take(27).collect::<String>())
            } else {
                title.to_string()
            },
            if r["assignee_user_id"].is_null() {
                "free"
            } else {
                "claimed"
            },
            labels.join(","),
        );
    }
    Ok(())
}

/// `nook issues get <key>` — one whole issue, the way an agent reads it.
pub async fn task(key: &str, json: bool, revisions: bool) -> Result<()> {
    let client = Client::from_config()?;
    if revisions {
        return task_revisions(&client, key, json).await;
    }
    let mut resp = client.get(&format!("/api/v1/tasks/{key}")).await?;
    // A second request rather than a wider detail payload: the ticket page has
    // its own attachment call already and does not want these bytes twice, and
    // the endpoint that answers for the ticket AND its comments at once exists
    // for exactly this reader (MAIN-534 AC-1).
    // `None` is "could not ask", which is not the same fact as an empty list —
    // a reader that could not tell them apart would treat an outage as a card
    // with no files, which is exactly the incomplete brief this ticket is about.
    let attachments = match crate::attachments::fetch(&client, key).await {
        Ok(rows) => Some(rows),
        Err(e) => {
            // Never fatal: the contract is the description, and a card that
            // would not print because its file list was unreachable helps
            // nobody.
            eprintln!(
                "{} could not read this card's attachments: {e}",
                crate::style::err("!")
            );
            None
        }
    };
    if json {
        // Additive, so an existing `--json` consumer is untouched — and the
        // agent-facing path is not the one that gets to be blind to a file
        // named in the description. Absent, not empty, when the ask failed.
        if let (Some(obj), Some(rows)) = (resp.as_object_mut(), attachments.clone()) {
            obj.insert("attachments".into(), Value::Array(rows));
        }
        println!("{}", serde_json::to_string_pretty(&resp)?);
        return Ok(());
    }
    let t = &resp["task"];
    println!(
        "{} {}",
        crate::style::bold(t["key"].as_str().unwrap_or("—")),
        t["title"].as_str().unwrap_or("")
    );
    let labels: Vec<&str> = t["labels"]
        .as_array()
        .map(|a| a.iter().filter_map(|l| l["name"].as_str()).collect())
        .unwrap_or_default();
    if !labels.is_empty() {
        println!("  labels: {}", labels.join(", "));
    }
    // The recorded PR is what tells a build run it is REPAIRING, not building
    // (MAIN-459 §2) — a card that only showed it on the --json path made every
    // repair run look like fresh work.
    if let Some(pr) = t["pr_url"].as_str().filter(|u| !u.is_empty()) {
        println!("  pr: {pr}");
    }
    if resp["is_blocked"].as_bool().unwrap_or(false) {
        let by: Vec<&str> = resp["blocked_by"]
            .as_array()
            .map(|a| a.iter().filter_map(|r| r["key"].as_str()).collect())
            .unwrap_or_default();
        println!("  {} {}", crate::style::err("BLOCKED by"), by.join(", "));
    }
    if let Some(d) = t["description"].as_str().filter(|d| !d.is_empty()) {
        println!();
        println!("{d}");
    }
    // Epic children (MAIN-89 AC-2): the tickets filed under this epic, with a
    // done count. `done` is a child in a completed OR canceled column.
    let children = resp["children"].as_array().cloned().unwrap_or_default();
    if t["type"].as_str() == Some("epic") || !children.is_empty() {
        let done = children
            .iter()
            .filter(|c| {
                matches!(
                    c["column_type"].as_str(),
                    Some("completed") | Some("canceled")
                )
            })
            .count();
        println!();
        println!(
            "{}",
            crate::style::dim(&format!("── Children · {done}/{} done", children.len()))
        );
        for c in &children {
            println!(
                "  {:<10} {:<12} {}",
                c["key"].as_str().unwrap_or("—"),
                c["column_type"].as_str().unwrap_or("—"),
                c["title"].as_str().unwrap_or(""),
            );
        }
    }
    let comments = resp["comments"].as_array().cloned().unwrap_or_default();
    if !comments.is_empty() {
        println!();
        println!(
            "{}",
            crate::style::dim(&format!("── {} comment(s)", comments.len()))
        );
        for c in &comments {
            println!(
                "\n{} {}",
                crate::style::bold(c["author_name"].as_str().unwrap_or("?")),
                crate::style::dim(c["created_at"].as_str().unwrap_or("")),
            );
            println!("{}", c["body_md"].as_str().unwrap_or(""));
        }
    }
    // A card with no files prints nothing extra at all (AC-8) — the section is
    // information, and a header saying "0 attachments" on every ticket in the
    // board is noise every reader then has to skip.
    if let Some(rows) = attachments.filter(|r| !r.is_empty()) {
        println!();
        for line in crate::attachments::render(key, &rows, &comments) {
            println!("{line}");
        }
    }
    let refs = resp["workspace_refs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for line in render_references(&refs) {
        println!("{line}");
    }
    Ok(())
}

/// The **References** section: the workspaces this card's description names
/// with `@slug` (MAIN-632 AC-3).
///
/// A reader — a human or a build agent — otherwise learns of the other side of
/// a cross-repo feature only from the `@slug` in the prose, with no way to tell
/// whether it resolved to anything. Naming the remote is what makes it
/// actionable: it is the repo the run may be given a read-only checkout of.
///
/// Empty in, nothing out: a header over an empty list on every card in the
/// board is noise every reader then has to skip, the same rule the attachment
/// section follows.
fn render_references(refs: &[Value]) -> Vec<String> {
    if refs.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![
        String::new(),
        crate::style::dim(&format!("── References · {}", refs.len())),
    ];
    for r in refs {
        let remote = r["git_remote_url"].as_str().unwrap_or("(no git remote)");
        lines.push(format!(
            "  @{:<20} {:<24} {remote}",
            r["slug"].as_str().unwrap_or("—"),
            r["name"].as_str().unwrap_or(""),
        ));
    }
    lines
}

/// `nook issues get <key> --revisions` — the description bodies past replaces
/// overwrote, newest first (MAIN-470 AC-3). This is the undo for a clobbered
/// description: read the body here, put it back with `set-description -`.
async fn task_revisions(client: &Client, key: &str, json: bool) -> Result<()> {
    let resp = client
        .get(&format!("/api/v1/tasks/{key}/revisions"))
        .await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
        return Ok(());
    }
    let rows = resp.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("no description revisions — the body has never been replaced");
        return Ok(());
    }
    println!(
        "{}",
        crate::style::dim(&format!(
            "── {} revision(s), newest first — each is the body a replace overwrote",
            rows.len()
        ))
    );
    for r in rows {
        println!(
            "\n{} {}",
            crate::style::bold(r["created_at"].as_str().unwrap_or("?")),
            crate::style::dim(r["author_id"].as_str().unwrap_or("(no user)")),
        );
        println!("{}", r["body"].as_str().unwrap_or(""));
    }
    Ok(())
}

/// `nook issues comment <key> [--unblock] [--request-changes] <body>` — where
/// the reasoning goes, and where a ruling that restarts a stopped card (MAIN-584
/// AC-9) or rejects its pull request (MAIN-591 AC-9) goes with it.
pub async fn comment(
    key: &str,
    argv: &[String],
    unblock: bool,
    request_changes: bool,
) -> Result<()> {
    // Only for a change request, which is the shape that carries a written
    // ruling and is routinely piped in. An ordinary `nook issues comment X -`
    // still means the one-character comment it always did (AC-9's second
    // sentence).
    let body = if request_changes {
        set_description_body(argv, || std::io::read_to_string(std::io::stdin()))?
    } else {
        argv.join(" ")
    };
    let client = Client::from_config()?;
    client
        .post(
            &format!("/api/v1/tasks/{key}/comments"),
            comment_body(&body, unblock, request_changes),
        )
        .await?;
    println!(
        "{} {} {}",
        crate::style::ok_c("✓"),
        match (unblock, request_changes) {
            (true, true) => "commented, requested changes and unblocked",
            (true, false) => "commented and unblocked",
            (false, true) => "commented and requested changes on",
            (false, false) => "commented on",
        },
        crate::style::bold(key)
    );
    Ok(())
}

/// The request, built apart from the send so a test can read it: without a
/// flag it carries neither `clear_escalation` nor `request_changes` at all,
/// which is what makes "an ordinary comment is unchanged" (MAIN-584 NG-4,
/// MAIN-591 AC-9) a property of the wire and not of a server-side default.
fn comment_body(body: &str, unblock: bool, request_changes: bool) -> serde_json::Value {
    let host = sysinfo::System::host_name().unwrap_or_else(|| "unknown".into());
    let mut req = serde_json::json!({
        "body_md": body,
        "author_name": format!("nook cli on {host}"),
    });
    if unblock {
        req["clear_escalation"] = serde_json::json!(true);
    }
    if request_changes {
        req["request_changes"] = serde_json::json!(true);
    }
    req
}

/// The argv body of `set-description`, honouring the Unix stdin convention
/// (MAIN-470 AC-1): a lone `-` means "read stdin" and is never content —
/// `nook issues create --description -` already reads it that way, and storing
/// the dash literally is exactly how a ticket's contract became the
/// one-character string `-`. Anything else is the joined argv, verbatim.
fn set_description_body(
    argv: &[String],
    stdin: impl FnOnce() -> std::io::Result<String>,
) -> Result<String> {
    if argv.len() == 1 && argv[0] == "-" {
        return stdin().context("reading the description from stdin");
    }
    Ok(argv.join(" "))
}

/// The sanity floor (MAIN-470 AC-2): shrinking a non-trivial description to a
/// near-empty body is almost always a lost payload, not an edit — refuse with
/// both sizes so the caller can see the mismatch, and let `--force` say it is
/// intentional.
fn tiny_replacement_refusal(current_len: usize, new_len: usize, force: bool) -> Option<String> {
    (!force && current_len > 200 && new_len < 20).then(|| {
        format!(
            "refusing to replace a {current_len}-char description with a {new_len}-char body — \
             this looks like payload loss, not an edit; pass --force if it is intentional"
        )
    })
}

/// `nook issues set-description <key> <body>` — replace a task's description
/// safely.
///
/// Read the current version, PATCH with the optimistic-concurrency guard, and
/// on a 409 (someone else edited it meanwhile) re-read and retry a bounded
/// number of times. If it keeps conflicting, exit non-zero rather than silently
/// losing the edit (AC-4).
pub async fn set_description(key: &str, argv: &[String], force: bool) -> Result<()> {
    let description = set_description_body(argv, || {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
        Ok(s)
    })?;
    let client = Client::from_config()?;
    for attempt in 1..=4 {
        // The current version to guard against — from the whole-issue read.
        let detail = client.get(&format!("/api/v1/tasks/{key}")).await?;
        let version = detail
            .get("task")
            .and_then(|t| t.get("updated_at"))
            .and_then(Value::as_str)
            .context("the task response carried no version to guard against")?
            .to_string();

        // Checked against the body this attempt would overwrite, so a retry
        // after a concurrent edit judges the description it actually replaces.
        let current_len = detail
            .get("task")
            .and_then(|t| t.get("description"))
            .and_then(Value::as_str)
            .map_or(0, |d| d.chars().count());
        if let Some(msg) = tiny_replacement_refusal(current_len, description.chars().count(), force)
        {
            bail!("{key}: {msg}");
        }

        let (status, body) = client
            .patch_status(
                &format!("/api/v1/tasks/{key}"),
                serde_json::json!({
                    "description": description,
                    "expected_updated_at": version,
                }),
            )
            .await?;
        match status {
            200 => {
                println!(
                    "{} updated {}",
                    crate::style::ok_c("✓"),
                    crate::style::bold(key)
                );
                return Ok(());
            }
            // Someone edited it between the read and the write — re-read the
            // fresh version and try again.
            409 => {
                eprintln!("  {key} changed under this edit — re-reading (attempt {attempt})");
                continue;
            }
            401 => bail!("unauthorized — this CLI's token was rejected"),
            other => bail!("{other} setting the description on {key}: {body}"),
        }
    }
    bail!("{key}: the body kept changing under concurrent edits — read it again and retry");
}

/// `nook issues label <key> <name> [--remove]`.
pub async fn label(key: &str, name: &str, remove: bool) -> Result<()> {
    let client = Client::from_config()?;
    let path = format!("/api/v1/tasks/{key}/labels/{name}");
    if remove {
        client.delete(&path).await?;
        println!("{} removed {name} from {key}", crate::style::ok_c("✓"));
    } else {
        client.put(&path, serde_json::json!({})).await?;
        println!("{} added {name} to {key}", crate::style::ok_c("✓"));
    }
    Ok(())
}

/// `nook issues claim <key>` — take the work.
pub async fn claim(key: &str, column_type: Option<&str>, any_workspace: bool) -> Result<()> {
    let client = Client::from_config()?;

    // The guard, and the reason it is here rather than only in the pick query:
    // the pick can be wrong — a stale filter, a hand-typed key, a skill edit —
    // and this is the last gate before an agent starts building. Inside a
    // workspace session, refuse a task that belongs to a different workspace (or
    // to none) unless the caller explicitly opts out. So even a mistaken pick
    // cannot become a feature built in the wrong repo.
    if !any_workspace {
        let scope = current_session_scope(&client).await?;
        if let Some(here) = scope.workspace() {
            let task = client.get(&format!("/api/v1/tasks/{key}")).await?;
            let task = task.get("task").unwrap_or(&task);
            let task_ws = task.get("workspace_id").and_then(|v| v.as_str());
            if claim_blocked(Some(&here.id), task_ws, any_workspace) {
                let theirs = match task_ws {
                    Some(_) => "a different workspace",
                    None => "no workspace",
                };
                bail!(
                    "{key} belongs to {theirs}; this session is in '{}'. \
                     Refusing so work isn't built in the wrong repo — pass \
                     --any-workspace to override.",
                    here.name
                );
            }
        }
    }

    let mut body = match column_type {
        Some(c) => serde_json::json!({ "column_type": c }),
        None => serde_json::json!({}),
    };
    // Which session this claim comes from (MAIN-142 AC-4). The control plane
    // uses it to refuse the build loop on a shared operator — a wall it can
    // only apply if it knows where the claim was typed. Absent outside a
    // session, which the server treats as "unknown", never as "allowed anyway".
    if let Ok(sid) = std::env::var("NOOK_SESSION_ID") {
        if !sid.is_empty() {
            body["session_id"] = serde_json::json!(sid);
        }
    }
    match client
        .post(&format!("/api/v1/tasks/{key}/claim"), body)
        .await
    {
        Ok(_) => {
            println!(
                "{} claimed {}",
                crate::style::ok_c("✓"),
                crate::style::bold(key)
            );
            Ok(())
        }
        // Losing the race is the expected outcome for all but one caller, so
        // it is reported as information rather than as a failure.
        Err(e) if e.to_string().contains("claimed this first") => {
            println!(
                "{} {key} was already taken — pick another",
                crate::style::dim("·")
            );
            Ok(())
        }
        Err(e) => Err(e),
    }
}

// ── notifications ───────────────────────────────────────────────────────────

/// `nook notify` — tell the fleet something happened.
///
/// One entry point for everything that wants to say something: an agent's
/// finish hook, a CI step, a cron job, a human. The control plane fans it out
/// to every connected UI and every configured channel, so the thing raising it
/// never has to know whether you read Slack.
///
/// Works with a NODE token as well as a user token — a machine reporting that
/// it finished is the whole point.
pub async fn notify_fleet(
    title: &str,
    body: Option<&str>,
    level: &str,
    kind: Option<&str>,
    link: Option<&str>,
    session: Option<&str>,
) -> Result<()> {
    anyhow::ensure!(!title.trim().is_empty(), "a notification needs a title");
    let client = Client::from_config()?;

    // Say where it came from without being asked. "Finished" is not useful on
    // a fleet; "finished on azul" is.
    let host = sysinfo::System::host_name().unwrap_or_else(|| "unknown".into());
    let mut payload = serde_json::json!({ "host": host });
    if let Ok(cwd) = std::env::current_dir() {
        payload["cwd"] = serde_json::json!(cwd.display().to_string());
    }
    // The session (from `$NOOK_SESSION_ID` in an agent hook) rides in the
    // payload too, so a client can act on it without re-parsing the link — the
    // control plane turns it into the actual deep-link URL.
    if let Some(s) = session.filter(|s| !s.is_empty()) {
        payload["session_id"] = serde_json::json!(s);
    }

    client
        .post(
            "/api/v1/notify",
            serde_json::json!({
                "title": title,
                "body": body,
                "level": level,
                "kind": kind.unwrap_or("cli"),
                "link": link,
                "session": session.filter(|s| !s.is_empty()),
                "payload": payload,
            }),
        )
        .await?;
    println!(
        "{} notified: {}",
        crate::style::ok_c("✓"),
        crate::style::bold(title)
    );
    Ok(())
}

// ── operator roles ──────────────────────────────────────────────────────────

/// `nook operator grant|revoke <email>` — who may run this deployment.
///
/// A deployment with one operator and no way to appoint another is one lost
/// password from being unadministrable, so this exists from the start rather
/// than waiting for a UI.
pub async fn operator_role(email: &str, role: &str, revoke: bool) -> Result<()> {
    let client = Client::from_config()?;
    client
        .post(
            "/api/v1/operator/bindings",
            serde_json::json!({ "email": email, "role": role, "revoke": revoke }),
        )
        .await?;
    println!(
        "{} {} {} @ deployment {} {}",
        crate::style::ok_c("✓"),
        if revoke { "revoked" } else { "granted" },
        crate::style::bold(role),
        if revoke { "from" } else { "to" },
        crate::style::bold(email),
    );
    Ok(())
}

/// `nook operator loops [on|off|status]` — the loop machinery's master switch
/// (MAIN-239).
///
/// Lives under `operator` rather than a new top-level verb (the CLI freeze),
/// because turning the fleet's loops on is a deployment act, not a per-user
/// preference. It writes the same tenant-scoped setting the Settings UI does,
/// and the control plane re-reads it every poll — so the change lands within a
/// poll interval with no restart.
/// The same switch for session reconciling (`sessions.reconcile.enabled`).
///
/// It is the twin of `operator_loops` and exists for the same reason: the
/// setting is tenant-scoped and defaults OFF, so a workspace can declare a
/// SessionSpec and converge never. Unlike loops, this one had NO way to change
/// it — no CLI verb, no UI write — which made declarative sessions reachable
/// only by hand-writing a PUT to /settings.
///
/// Deliberately a separate verb rather than a flag on `loops`: they gate
/// different machinery (job dispatch vs session convergence) and a deployment
/// commonly wants one without the other.
pub async fn operator_reconcile(state: &str) -> Result<()> {
    operator_switch(
        state,
        "sessions.reconcile.enabled",
        "reconciling",
        "  Declared workspaces start converging within a poll interval.",
        "  Managed sessions are left alone; nothing is torn down.",
    )
    .await
}

/// The shared body of the two tenant switches, so their behaviour — the accepted
/// words, the "off (default)" reading of an absent setting, the exit codes —
/// cannot drift apart.
async fn operator_switch(
    state: &str,
    key: &str,
    label: &str,
    on_note: &str,
    off_note: &str,
) -> Result<()> {
    let client = Client::from_config()?;

    let want = match state {
        "on" | "enable" | "enabled" => Some(true),
        "off" | "disable" | "disabled" => Some(false),
        "status" | "" => None,
        other => {
            anyhow::bail!("unknown state {other:?} — expected on, off, or status");
        }
    };

    if let Some(on) = want {
        client
            .put(
                &format!("/api/v1/settings/{key}"),
                serde_json::json!({ "scope": "tenant", "value": on }),
            )
            .await?;
        println!(
            "{label} {}",
            if on {
                crate::style::ok_c("enabled")
            } else {
                crate::style::dim("disabled")
            }
        );
        println!("{}", if on { on_note } else { off_note });
        return Ok(());
    }

    // An absent setting is the default — say "off (default)" rather than a bare
    // "off", so nobody hunts for a switch they never flipped.
    let settings = client.get("/api/v1/settings").await?;
    let row = settings.as_array().and_then(|a| {
        a.iter()
            .find(|s| s["key"].as_str() == Some(key) && s["scope"].as_str() == Some("tenant"))
    });
    match row {
        Some(r) if r["value"].as_bool() == Some(true) => {
            println!("{label} {}", crate::style::ok_c("enabled"))
        }
        Some(_) => println!("{label} {}", crate::style::dim("disabled")),
        None => println!(
            "{label} {} — no setting stored yet",
            crate::style::dim("disabled (default)")
        ),
    }
    Ok(())
}

pub async fn operator_loops(state: &str) -> Result<()> {
    let client = Client::from_config()?;

    let want = match state {
        "on" | "enable" | "enabled" => Some(true),
        "off" | "disable" | "disabled" => Some(false),
        "status" | "" => None,
        other => {
            anyhow::bail!("unknown state {other:?} — expected on, off, or status");
        }
    };

    if let Some(on) = want {
        client
            .put(
                "/api/v1/settings/loops.enabled",
                serde_json::json!({ "scope": "tenant", "value": on }),
            )
            .await?;
        println!(
            "loops {}",
            if on {
                crate::style::ok_c("enabled")
            } else {
                crate::style::dim("disabled")
            }
        );
        if on {
            println!("  Queued jobs are picked up within a poll interval.");
        } else {
            println!("  Queued jobs stay queued; nothing is lost.");
        }
        return Ok(());
    }

    // Status. An absent setting is the default — say "off (default)" rather
    // than a bare "off", so nobody hunts for a switch they never flipped.
    let settings = client.get("/api/v1/settings").await?;
    let row = settings.as_array().and_then(|a| {
        a.iter().find(|s| {
            s["key"].as_str() == Some("loops.enabled") && s["scope"].as_str() == Some("tenant")
        })
    });
    match row {
        Some(r) if r["value"].as_bool() == Some(true) => {
            println!("loops {}", crate::style::ok_c("enabled"))
        }
        Some(_) => println!("loops {}", crate::style::dim("disabled")),
        None => println!(
            "loops {} — no setting stored yet",
            crate::style::dim("disabled (default)")
        ),
    }
    println!("  Change with: nook operator loops on|off");
    Ok(())
}

/// `nook operator who` — who holds what, so "why can't I see that" has an
/// answer that does not require reading the database.
pub async fn operator_who() -> Result<()> {
    let client = Client::from_config()?;
    let me = client.get("/api/v1/auth/me").await?;
    let cap = &me["capability"];
    println!(
        "you:      {} ({})",
        me["user"]["email"].as_str().unwrap_or("?"),
        me["tenant"]["slug"].as_str().unwrap_or("?")
    );
    println!(
        "operator: {}",
        if cap["operator"].as_bool().unwrap_or(false) {
            crate::style::ok_c("yes")
        } else {
            crate::style::dim("no")
        }
    );
    let held: Vec<&str> = cap["deployment"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    if held.is_empty() {
        println!(
            "held:     {}",
            crate::style::dim("nothing at deployment scope")
        );
    } else {
        println!("held:     {}", held.join(", "));
    }
    Ok(())
}

/// `nook operator bindings` — who holds what.
pub async fn operator_bindings(json: bool) -> Result<()> {
    let client = Client::from_config()?;
    let rows = client.get("/api/v1/operator/bindings").await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    // The endpoint returns a page (`{rows, next_cursor}`), not a bare array —
    // reading the body as an array printed "No role bindings." unconditionally.
    let rows = rows
        .get("rows")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        println!("No role bindings.");
        return Ok(());
    }
    println!("{:<26} {:<14} {:<12} WHERE", "WHO", "ROLE", "SCOPE");
    for r in rows {
        println!(
            "{:<26} {:<14} {:<12} {}",
            r["email"].as_str().unwrap_or("?"),
            r["role_key"].as_str().unwrap_or("?"),
            r["scope_type"].as_str().unwrap_or("?"),
            r["scope_label"].as_str().unwrap_or("—"),
        );
    }
    Ok(())
}

/// `nook operator orgs` and the writes that go with it.
pub async fn operator_orgs(json: bool) -> Result<()> {
    let client = Client::from_config()?;
    let rows = client.get("/api/v1/operator/orgs").await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    println!("{:<24} {:<24} TENANTS", "NAME", "SLUG");
    for r in rows.as_array().cloned().unwrap_or_default() {
        println!(
            "{:<24} {:<24} {}",
            r["name"].as_str().unwrap_or("?"),
            r["slug"].as_str().unwrap_or("?"),
            r["tenants"].as_i64().unwrap_or(0),
        );
    }
    Ok(())
}

pub async fn operator_org_create(name: &str, slug: Option<&str>) -> Result<()> {
    let client = Client::from_config()?;
    let r = client
        .post(
            "/api/v1/operator/orgs",
            serde_json::json!({ "name": name, "slug": slug }),
        )
        .await?;
    println!(
        "{} created org {}",
        crate::style::ok_c("✓"),
        crate::style::bold(r["slug"].as_str().unwrap_or(name))
    );
    Ok(())
}

/// `nook interactions ask <prompt>` — raise a durable question for a human.
///
/// The ask is persisted server-side and answerable from any surface, so with
/// `--wait` this process can block on a human decision across a dropped
/// connection without losing it. Auto-scopes to the calling session/job from
/// `NOOK_SESSION_ID` / `NOOK_JOB_ID` when the flags are not passed.
pub async fn interactions_ask(
    prompt: &str,
    choices: &[String],
    wait: bool,
    job: Option<&str>,
    task: Option<&str>,
) -> Result<()> {
    let client = Client::from_config()?;

    let mut body = serde_json::Map::new();
    body.insert("prompt".into(), Value::String(prompt.to_string()));
    if !choices.is_empty() {
        body.insert("choices".into(), serde_json::json!(choices));
    }
    // A flag wins; otherwise fall back to the environment an in-session
    // executor exports, so its ask auto-anchors to its own job.
    if let Some(job_id) = job
        .map(str::to_string)
        .or_else(|| std::env::var("NOOK_JOB_ID").ok())
    {
        body.insert("job_id".into(), Value::String(job_id));
    }
    if let Some(task_id) = task.map(str::to_string) {
        body.insert("task_id".into(), Value::String(task_id));
    }
    if let Ok(session_id) = std::env::var("NOOK_SESSION_ID") {
        body.insert("session_id".into(), Value::String(session_id));
    }

    let r = client
        .post("/api/v1/interactions", Value::Object(body))
        .await?;
    let interaction: nook_types::Interaction =
        serde_json::from_value(r).context("unexpected interaction response")?;
    let id = interaction.id.to_string();
    println!(
        "{} asked {}",
        crate::style::ok_c("✓"),
        crate::style::bold(&id)
    );

    if !wait {
        return Ok(());
    }

    // Poll for the answer. A human's decision can take a while, so the deadline
    // is generous; a fixed 2s interval mirrors `exec`.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60 * 60);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let r = client.get(&format!("/api/v1/interactions/{id}")).await?;
        let current: nook_types::Interaction =
            serde_json::from_value(r).context("unexpected interaction response")?;
        match current.state.as_str() {
            "pending" => continue,
            "canceled" => {
                println!("{} interaction was canceled", crate::style::dim("·"));
                return Ok(());
            }
            _ => {
                let answer = current.response.unwrap_or_default();
                println!("{}", crate::style::reply(&answer));
                // Plain to stdout as well, so a caller/script can capture it.
                println!("{answer}");
                return Ok(());
            }
        }
    }

    println!(
        "{} timed out waiting for an answer — it is still pending as {}",
        crate::style::dim("·"),
        id
    );
    Ok(())
}

/// `nook interactions answer <id> <response>` — resolve a pending ask.
pub async fn interactions_answer(id: &str, response: &str) -> Result<()> {
    let client = Client::from_config()?;
    client
        .post(
            &format!("/api/v1/interactions/{id}/answer"),
            serde_json::json!({ "response": response }),
        )
        .await?;
    println!(
        "{} answered {}",
        crate::style::ok_c("✓"),
        crate::style::bold(id)
    );
    Ok(())
}

/// Stage a new certificate authority for a tenant.
///
/// Staging only, deliberately. Machines learn the new CA on their next renewal;
/// promoting it before they have would strand every node that had not. Promote
/// as a second, later act.
pub async fn operator_ca_stage(tenant: &str) -> Result<()> {
    let client = Client::from_config()?;
    let r = client
        .post(
            &format!("/api/v1/operator/tenants/{tenant}/ca"),
            serde_json::json!({}),
        )
        .await?;
    println!(
        "{} staged a CA for {}",
        crate::style::ok_c("✓"),
        crate::style::bold(tenant)
    );
    println!("  id:          {}", r["id"].as_str().unwrap_or("?"));
    println!(
        "  fingerprint: {}",
        r["fingerprint"].as_str().unwrap_or("?")
    );
    println!();
    println!(
        "{}",
        crate::style::dim("Nodes pick this up on their next renewal. Promote it once they have:")
    );
    println!(
        "    nook operator ca promote {tenant} {}",
        r["id"].as_str().unwrap_or("<id>")
    );
    Ok(())
}

pub async fn operator_ca_promote(tenant: &str, ca: &str) -> Result<()> {
    let client = Client::from_config()?;
    client
        .post(
            &format!("/api/v1/operator/tenants/{tenant}/ca/{ca}/promote"),
            serde_json::json!({}),
        )
        .await?;
    println!(
        "{} {} now signs for {}",
        crate::style::ok_c("✓"),
        crate::style::bold(ca),
        crate::style::bold(tenant)
    );
    Ok(())
}

/// Revoke a node's certificate, or remove it entirely.
pub async fn operator_node(node: &str, remove: bool) -> Result<()> {
    let client = Client::from_config()?;
    if remove {
        client
            .delete(&format!("/api/v1/operator/nodes/{node}"))
            .await?;
        println!(
            "{} removed node {}",
            crate::style::ok_c("✓"),
            crate::style::bold(node)
        );
    } else {
        client
            .post(
                &format!("/api/v1/operator/nodes/{node}/revoke"),
                serde_json::json!({}),
            )
            .await?;
        println!(
            "{} revoked {} — it can no longer connect",
            crate::style::ok_c("✓"),
            crate::style::bold(node)
        );
    }
    Ok(())
}

/// Move a tenant into another org.
pub async fn operator_move_tenant(tenant: &str, org: &str) -> Result<()> {
    let client = Client::from_config()?;
    client
        .post(
            &format!("/api/v1/operator/tenants/{tenant}/org"),
            serde_json::json!({ "org_id": org }),
        )
        .await?;
    println!(
        "{} moved {} into org {}",
        crate::style::ok_c("✓"),
        crate::style::bold(tenant),
        crate::style::bold(org)
    );
    Ok(())
}

#[cfg(test)]
mod claim_guard_tests {
    use super::*;

    const NOOK: &str = "11111111-1111-1111-1111-111111111111";
    const OTHER: &str = "22222222-2222-2222-2222-222222222222";

    /// A task in this session's own workspace is claimable.
    #[test]
    fn same_workspace_is_allowed() {
        assert!(!claim_blocked(Some(NOOK), Some(NOOK), false));
    }

    /// The whole point: a task in another repo is refused from a confined
    /// session, so an agent cannot build another repo's ticket from this one.
    #[test]
    fn a_different_workspace_is_refused() {
        assert!(claim_blocked(Some(NOOK), Some(OTHER), false));
    }

    /// An unscoped task is refused too — a confined loop must not adopt work
    /// nobody assigned to a repo (decided: own workspace only).
    #[test]
    fn an_unscoped_task_is_refused() {
        assert!(claim_blocked(Some(NOOK), None, false));
    }

    /// `--any-workspace` turns the guard off for every case.
    #[test]
    fn the_override_allows_anything() {
        assert!(!claim_blocked(Some(NOOK), Some(OTHER), true));
        assert!(!claim_blocked(Some(NOOK), None, true));
    }

    /// Outside a workspace session there is nothing to confine to, so a human
    /// running `nook issues claim` by hand is never blocked.
    #[test]
    fn no_session_workspace_never_blocks() {
        assert!(!claim_blocked(None, Some(OTHER), false));
        assert!(!claim_blocked(None, None, false));
    }
}

/// `nook issues get <KEY>`'s References section (MAIN-632 AC-3).
#[cfg(test)]
mod task_reference_tests {
    use super::render_references;
    use serde_json::json;

    #[test]
    fn a_card_with_references_lists_name_slug_and_remote() {
        let out = render_references(&[
            json!({
                "workspace_id": "0199-web",
                "name": "Nook Web",
                "slug": "nook-web",
                "git_remote_url": "git@example.test:acme/web.git",
            }),
            json!({
                "workspace_id": "0199-api",
                "name": "Nook API",
                "slug": "nook-api",
                "git_remote_url": null,
            }),
        ])
        .join("\n");

        assert!(out.contains("References"), "{out}");
        assert!(out.contains("@nook-web"), "{out}");
        assert!(out.contains("Nook Web"), "{out}");
        assert!(out.contains("git@example.test:acme/web.git"), "{out}");
        // A workspace nobody has given a remote is still worth naming — the
        // reference resolved, which is the fact the reader came for.
        assert!(out.contains("@nook-api"), "{out}");
        assert!(out.contains("(no git remote)"), "{out}");
    }

    /// Nearly every card names nothing, and a header over an empty list on
    /// every ticket in the board is noise every reader then has to skip — the
    /// same rule the attachment section follows.
    #[test]
    fn a_card_with_no_references_prints_nothing() {
        assert!(render_references(&[]).is_empty());
    }
}

#[cfg(test)]
mod set_description_guard_tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// MAIN-470 AC-1: a lone `-` is the stdin convention, never content — the
    /// exact payload that replaced a ticket's contract with one character.
    #[test]
    fn a_lone_dash_reads_stdin_and_is_never_stored() {
        let body =
            set_description_body(&args(&["-"]), || Ok("piped body\n".into())).expect("stdin body");
        assert_eq!(body, "piped body\n");
    }

    /// Ordinary argv is joined verbatim, and stdin is never touched.
    #[test]
    fn ordinary_argv_is_joined_without_touching_stdin() {
        let body = set_description_body(&args(&["hello", "world"]), || {
            panic!("stdin must not be read for a literal body")
        })
        .expect("literal body");
        assert_eq!(body, "hello world");
    }

    /// Only the LONE dash is the convention: a dash among other words is
    /// content (a real body could open with "- item one").
    #[test]
    fn a_dash_among_words_is_literal_content() {
        let body = set_description_body(&args(&["-", "item", "one"]), || {
            panic!("stdin must not be read")
        })
        .expect("literal body");
        assert_eq!(body, "- item one");
    }

    /// MAIN-470 AC-2: the floor refuses a probable payload loss, naming both
    /// sizes so the mismatch is visible.
    #[test]
    fn shrinking_a_real_description_to_a_stub_is_refused_naming_both_sizes() {
        let msg = tiny_replacement_refusal(350, 5, false)
            .expect("a 350 -> 5 char replace must be refused");
        assert!(msg.contains("350"), "names the current size: {msg}");
        assert!(msg.contains("5-char"), "names the new size: {msg}");
        assert!(msg.contains("--force"), "names the override: {msg}");
    }

    /// `--force` says the shrink is intentional; the floor steps aside.
    #[test]
    fn force_overrides_the_floor() {
        assert_eq!(tiny_replacement_refusal(350, 5, true), None);
    }

    /// The floor only bites on BOTH conditions: a short current body may be
    /// replaced freely, and a substantial new body is a rewrite, not a loss.
    #[test]
    fn ordinary_edits_pass_the_floor() {
        assert_eq!(tiny_replacement_refusal(100, 5, false), None);
        assert_eq!(tiny_replacement_refusal(350, 50, false), None);
        assert_eq!(tiny_replacement_refusal(0, 5, false), None);
    }
}

#[cfg(test)]
mod table_tests {
    use super::*;
    use serde_json::json;

    /// The whole point of the dotted path: the fields worth showing about a
    /// node live under `capabilities`, and a table that could only read
    /// top-level keys showed none of them.
    #[test]
    fn a_dotted_path_reaches_into_nested_objects() {
        let row = json!({
            "name": "crimson",
            "capabilities": { "agent_version": "0.4.3", "cpus": 32 }
        });
        assert_eq!(cell(&row, "name"), "crimson");
        assert_eq!(cell(&row, "capabilities.agent_version"), "0.4.3");
        assert_eq!(cell(&row, "capabilities.cpus"), "32");
    }

    /// A node too old to report its version, and one that reports nothing at
    /// all, must both read as "-" rather than panicking or printing `null`.
    #[test]
    fn a_missing_path_is_a_dash_at_every_depth() {
        let row = json!({ "name": "amber", "capabilities": { "agent_version": null } });
        assert_eq!(cell(&row, "capabilities.agent_version"), "-");
        assert_eq!(cell(&row, "capabilities.nope"), "-");
        assert_eq!(cell(&row, "nope.nope.nope"), "-");
        assert_eq!(cell(&json!({}), "capabilities.cpus"), "-");
    }

    /// Bytes and JSON arrays are the two things that made this table
    /// unreadable: `51539607552` and `["bash","zsh"]` are both wider than the
    /// column they sit in and neither is what a person wants to read.
    #[test]
    fn sizes_and_lists_are_rendered_for_people() {
        let row = json!({
            "capabilities": { "memory": 51539607552_i64, "runtimes": ["claude", "bash"] }
        });
        assert_eq!(cell(&row, "capabilities.memory"), "48G");
        assert_eq!(cell(&row, "capabilities.runtimes"), "claude,bash");
        // An empty list is nothing, not "[]".
        assert_eq!(
            cell(
                &json!({"capabilities": {"runtimes": []}}),
                "capabilities.runtimes"
            ),
            "-"
        );
    }

    /// AC-3: a draining node has to be distinguishable from an idle one in the
    /// table, and an overdue one from a draining one. A node taking work says
    /// nothing at all, because most of them are.
    #[test]
    fn a_cordoned_node_reads_as_updating_and_an_overdue_one_is_marked() {
        assert_eq!(cell(&json!({ "name": "azul" }), "cordon"), "-");
        assert_eq!(cell(&json!({ "cordon": null }), "cordon"), "-");
        assert_eq!(
            cell(
                &json!({ "cordon": { "jobs_in_flight": 2, "overdue": false } }),
                "cordon"
            ),
            "updating 2 jobs"
        );
        assert_eq!(
            cell(
                &json!({ "cordon": { "jobs_in_flight": 1, "overdue": true } }),
                "cordon"
            ),
            "updating 1 job !"
        );
    }

    /// The header names the field, not where it is stored — a column headed
    /// `CAPABILITIES.AGENT_VERSION` is a path, and paths are for the code.
    #[test]
    fn headers_drop_the_path() {
        let cols = columns("nodes", &json!({}));
        let headers: Vec<String> = cols
            .iter()
            .map(|c| c.rsplit('.').next().unwrap_or(c).to_uppercase())
            .collect();
        assert!(headers.contains(&"AGENT_VERSION".to_string()));
        assert!(
            !headers.iter().any(|h| h.contains('.')),
            "no header should carry a dotted path: {headers:?}"
        );
    }
}

#[cfg(test)]
mod task_verb_tests {
    use super::*;

    #[test]
    fn tasks_query_maps_type_and_parent() {
        // `--type epic` (repeatable) and `--parent` reach the query verbatim
        // (MAIN-89 AC-1) — the server lifts the epic/backlog exclusions for them.
        let q = build_tasks_query(
            Some("MAIN"),
            Some("ws-1"),
            &["agent-ready".into()],
            &["blocked".into()],
            Some("none"),
            None,
            &["epic".into(), "bug".into()],
            Some("NOOK-7"),
            true,
            Some("node-9"),
            true,
            true,
        );
        assert!(
            q.contains(&"node=node-9".to_string()),
            "--this-node must reach the query, or dispatch means nothing again"
        );
        assert!(q.contains(&"type=epic".to_string()));
        assert!(q.contains(&"type=bug".to_string()));
        assert!(q.contains(&"parent=NOOK-7".to_string()));
        assert!(q.contains(&"board=MAIN".to_string()));
        assert!(q.contains(&"workspace=ws-1".to_string()));
        assert!(q.contains(&"label=agent-ready".to_string()));
        assert!(q.contains(&"not_label=blocked".to_string()));
        assert!(q.contains(&"assignee=none".to_string()));
        assert!(q.contains(&"is_blocked=false".to_string()));
        assert!(q.contains(&"backlog=true".to_string()));
        assert!(q.contains(&"done=true".to_string()));
    }

    #[test]
    fn tasks_query_omits_unset_filters() {
        // No workspace, no type, no parent → none of those keys appear, so a
        // bare `nook issues list` hits `/api/v1/tasks` with nothing extra.
        let q = build_tasks_query(
            None,
            None,
            &[],
            &[],
            None,
            None,
            &[],
            None,
            false,
            None,
            false,
            false,
        );
        assert!(q.is_empty(), "an unfiltered query is empty: {q:?}");
    }

    fn opts() -> CreateTask {
        CreateTask {
            title: "cli test".into(),
            board: None,
            description: None,
            column_type: None,
            priority: Some(2),
            labels: vec!["test".into()],
            type_: Some("bug".into()),
            parent: Some("NOOK-7".into()),
            workspace: None,
        }
    }

    #[test]
    fn create_body_uses_wire_names_and_omits_unset() {
        // Every flag lands under its wire name (`type`, `parent`, `labels`,
        // `workspace_id`); the description is resolved by the caller (AC-3).
        let body = build_create_body(&opts(), Some("# body".into()), Some("ws-9".into()));
        assert_eq!(body["title"], "cli test");
        assert_eq!(body["description"], "# body");
        assert_eq!(body["priority"], 2);
        assert_eq!(body["type"], "bug"); // wire name is `type`, not `type_`
        assert_eq!(body["parent"], "NOOK-7");
        assert_eq!(body["workspace_id"], "ws-9");
        assert_eq!(body["labels"], serde_json::json!(["test"]));
        // Unset stays absent so the server's defaults apply.
        assert!(body.get("column_type").is_none());
    }

    #[test]
    fn create_body_without_optionals_is_just_the_title() {
        let bare = CreateTask {
            title: "t".into(),
            board: None,
            description: None,
            column_type: None,
            priority: None,
            labels: vec![],
            type_: None,
            parent: None,
            workspace: None,
        };
        let body = build_create_body(&bare, None, None);
        assert_eq!(body["title"], "t");
        assert_eq!(body.as_object().unwrap().len(), 1, "only title: {body}");
    }
}

/// `nook get workspace git-ssh …` — the ssh shim git runs (MAIN-367).
///
/// Sessions export `GIT_SSH_COMMAND` pointing here, so every git command inside
/// one — typed by a person, or run by a loop agent that knows nothing about any
/// of this — authenticates with the workspace's own key. An agent cannot forget
/// to fetch a credential it never has to ask for.
///
/// The key is written for the length of ONE ssh invocation and removed in the
/// same breath. Nothing persists on the node, which is the property that lets a
/// shared operator machine clone a private repo without becoming a place where
/// private keys accumulate. `TempKey`'s Drop is what guarantees it, so an ssh
/// that fails, is killed, or panics still cleans up.
///
/// Falls through to plain ssh whenever there is no key to use — outside a nook
/// session, in an ad-hoc terminal, or for a workspace that pins nothing. That is
/// the ordinary case and it must keep working untouched: public repos and local
/// paths have never needed a credential.
pub async fn git_ssh(args: &[String]) -> Result<()> {
    let key = fetch_session_git_key().await;
    let held = key.as_deref().and_then(TempKey::write);

    let mut cmd = std::process::Command::new("ssh");
    if let Some(k) = &held {
        // `IdentitiesOnly` so a stray agent identity cannot silently answer for
        // a repo this key was chosen for.
        cmd.args(["-i", &k.path.to_string_lossy(), "-o", "IdentitiesOnly=yes"]);
    }
    cmd.args(args);
    let status = cmd.status().context("could not run ssh")?;
    // Dropping `held` removes the key before this process exits, whatever ssh did.
    drop(held);
    std::process::exit(status.code().unwrap_or(1));
}

/// The workspace credential for the repo this session is in, or `None`.
///
/// Asked as NODE + WORKSPACE, not as a session (MAIN-367 review). A git
/// credential is workspace data, not session content: routing it through a
/// session forced the fetch onto the session-content authorization path, and a
/// node running another tenant's workspace could only be let through there by
/// widening that guard for every session route. Both ids are already to hand —
/// the node knows itself from its config, and the session exports
/// `NOOK_WORKSPACE_ID` — so this needs no lookup at all.
///
/// Falling back to the node's own key is correct for a workspace that pins
/// nothing, and a lie for a workspace that pins one we could not fetch — but
/// both used to be a silent `None`. That silence hid two real defects during
/// this ticket's own review: a cross-tenant 403, and a loop session missing its
/// workspace id. In both, git simply used the wrong key and failed later with an
/// authentication error nobody could trace back here.
///
/// So the fallback stays — a control plane that is briefly unreachable must not
/// break `git status` — but it announces itself. `git` shows a
/// `GIT_SSH_COMMAND`'s stderr, so one line reaches whoever ran the command, and
/// a loop transcript keeps it.
async fn fetch_session_git_key() -> Option<String> {
    // Not in a workspace session at all: an ad-hoc terminal, or a shell outside
    // nook. Nothing is expected here, so nothing is said.
    let workspace = std::env::var("NOOK_WORKSPACE_ID")
        .ok()
        .filter(|s| !s.is_empty())?;

    let outcome = match load_client_for_git_key() {
        Err(why) => Err(why),
        Ok((client, node)) => client
            .get_text(&format!(
                "/api/v1/nodes/{node}/workspaces/{workspace}/git-key"
            ))
            .await
            .map_err(|e| e.to_string()),
    };
    let (key, warning) = classify_git_key(outcome);
    if let Some(warning) = warning {
        eprintln!("nook: {warning}; falling back to this machine's own key");
    }
    key
}

/// The config and credential this machine fetches with, or why it cannot.
fn load_client_for_git_key() -> Result<(Client, String), String> {
    let node = NodeConfig::load()
        .map_err(|e| format!("this machine has no node config ({e})"))?
        .node_id;
    // `as_this_node`, not `from_config`: the git-key route is machine-only, so
    // the node's own credential is the only one it accepts. `from_config` would
    // hand over a `nook login` user token wherever one exists — which is most
    // machines a human has touched — and earn a 403 for it.
    let client = Client::as_this_node().map_err(|e| format!("no usable credential ({e})"))?;
    Ok((client, node))
}

/// What a fetch outcome means: the key to use, and anything worth saying.
///
/// Pure, so the distinction that matters — "nothing is pinned" versus "we could
/// not find out" — is asserted by tests rather than trusted.
fn classify_git_key(outcome: Result<String, String>) -> (Option<String>, Option<String>) {
    match outcome {
        // A 204 comes back as an empty body: this workspace pins no credential,
        // which is the ordinary case and is not worth a word.
        Ok(body) if body.trim().is_empty() => (None, None),
        Ok(body) => (Some(body), None),
        Err(why) => (
            None,
            Some(format!("could not fetch this workspace's git key: {why}")),
        ),
    }
}

/// A private key on disk for exactly as long as one command needs it.
struct TempKey {
    path: std::path::PathBuf,
}

impl TempKey {
    fn write(material: &str) -> Option<Self> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = std::env::temp_dir().join(format!("nook-git-{}", uuid::Uuid::now_v7()));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            // 0600 at CREATION, not after: a key that is briefly world-readable
            // on a shared operator machine is a key that leaked.
            .mode(0o600)
            .open(&path)
            .ok()?;
        let mut body = material.trim_end().to_string();
        body.push('\n');
        f.write_all(body.as_bytes()).ok()?;
        Some(Self { path })
    }
}

impl Drop for TempKey {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod git_ssh_tests {
    use super::{classify_git_key, TempKey};
    use std::os::unix::fs::PermissionsExt;

    /// The distinction that was missing. "Nothing pinned" and "we could not find
    /// out" both fell back to the node's own key, and both said nothing — which
    /// hid a cross-tenant 403 and a loop session with no workspace id during this
    /// ticket's own review. Falling back is still right; being quiet about it is
    /// not.
    #[test]
    fn an_unpinned_workspace_is_silent_but_a_failure_speaks() {
        // 204 → empty body → the ordinary case, no key and no noise.
        assert_eq!(classify_git_key(Ok(String::new())), (None, None));
        assert_eq!(classify_git_key(Ok("   \n".into())), (None, None));

        // A real failure still falls back, but says so.
        let (key, warning) = classify_git_key(Err("403 Forbidden".into()));
        assert!(key.is_none(), "a failed fetch must not invent a key");
        let warning = warning.expect("a failure must be announced");
        assert!(
            warning.contains("403"),
            "the warning must carry the cause, got: {warning}"
        );
    }

    #[test]
    fn a_fetched_key_is_returned_verbatim_and_quietly() {
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n";
        let (key, warning) = classify_git_key(Ok(pem.to_string()));
        assert_eq!(key.as_deref(), Some(pem));
        assert!(warning.is_none());
    }

    /// 0600 at creation, not after. A key that is briefly world-readable on a
    /// shared operator machine is a key that leaked (MAIN-367 AC-7).
    #[test]
    fn the_key_is_never_readable_by_anyone_else() {
        let held = TempKey::write("PRIVATE KEY MATERIAL").expect("wrote the key");
        let mode = std::fs::metadata(&held.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    /// The whole "nothing persists on the node" property rests on Drop, so it
    /// has to hold when ssh fails or is killed — not only on the happy path.
    #[test]
    fn the_key_is_gone_once_the_command_is_over() {
        let path = {
            let held = TempKey::write("PRIVATE KEY MATERIAL").expect("wrote the key");
            assert!(held.path.exists());
            held.path.clone()
        };
        assert!(
            !path.exists(),
            "the key outlived the command that needed it"
        );
    }

    /// Two concurrent git commands in one session must not share, or clobber,
    /// each other's file — `create_new` plus a v7 id is what prevents that.
    #[test]
    fn concurrent_commands_get_their_own_file() {
        let a = TempKey::write("A").expect("a");
        let b = TempKey::write("B").expect("b");
        assert_ne!(a.path, b.path);
        assert_eq!(std::fs::read_to_string(&a.path).unwrap().trim(), "A");
        assert_eq!(std::fs::read_to_string(&b.path).unwrap().trim(), "B");
    }
}

#[cfg(test)]
mod tests {
    use super::unique_id_len;

    /// MAIN-584 NG-4 and MAIN-591 AC-9: the flags are the whole difference on
    /// the wire. Without them the request must carry neither field at all, so
    /// a question can still be asked on a stopped card from the CLI.
    #[test]
    fn only_the_flags_put_their_fields_on_the_wire() {
        let plain = super::comment_body("just asking", false, false);
        assert_eq!(plain["body_md"], "just asking");
        assert!(
            plain.get("clear_escalation").is_none() && plain.get("request_changes").is_none(),
            "an ordinary comment is unchanged: {plain}"
        );

        let ruling = super::comment_body("build it as specified", true, false);
        assert_eq!(ruling["clear_escalation"], serde_json::json!(true));
        assert!(ruling.get("request_changes").is_none());

        let rejection = super::comment_body("AC-2 is not met", false, true);
        assert_eq!(rejection["request_changes"], serde_json::json!(true));
        assert!(rejection.get("clear_escalation").is_none());

        // Both is valid and does both — they are independent (AC-1).
        let both = super::comment_body("ruled, and fix the resolver", true, true);
        assert_eq!(both["clear_escalation"], serde_json::json!(true));
        assert_eq!(both["request_changes"], serde_json::json!(true));
    }

    /// MAIN-618 AC-6. The three states this column has to tell apart: a node
    /// with room, a node the floor has stopped, and a node that has not said.
    #[test]
    fn the_disk_column_says_which_node_is_held_back() {
        const GB: u64 = 1024 * 1024 * 1024;
        let node = |disks: serde_json::Value, shortage: serde_json::Value| serde_json::json!({ "resources": { "disks": disks, "disk_shortage": shortage } });
        let roomy = node(
            serde_json::json!([{ "label": "job cache", "mount_point": "/",
                                 "free_bytes": 120 * GB, "total_bytes": 500 * GB }]),
            serde_json::Value::Null,
        );
        assert_eq!(super::cell(&roomy, "disk"), "120G");

        // The TIGHTEST filesystem is the one shown: the roomy one cannot lift
        // a gate the other imposed.
        let short = node(
            serde_json::json!([
                { "label": "job cache", "mount_point": "/home",
                  "free_bytes": 200 * GB, "total_bytes": 500 * GB },
                { "label": "Docker data root", "mount_point": "/var/lib/docker",
                  "free_bytes": 3 * GB, "total_bytes": 100 * GB },
            ]),
            serde_json::json!("below the 20.0 GiB free-disk floor: …"),
        );
        assert_eq!(super::cell(&short, "disk"), "3G LOW");

        // An agent that predates the field, which is NOT the same as a full
        // one — the dispatcher does not gate it, and this must not imply it.
        let silent = serde_json::json!({ "resources": { "cpu_percent": 4.0 } });
        assert_eq!(super::cell(&silent, "disk"), "-");
    }

    /// MAIN-647 AC-5: the listing tells "takes no loop work" apart from "has
    /// nothing to do". Both were an idle-looking row before this.
    #[test]
    fn the_loops_column_says_which_node_would_refuse_everything() {
        let node = |kinds: serde_json::Value| serde_json::json!({ "capabilities": { "loop_kinds": kinds } });
        assert_eq!(
            super::cell(&node(serde_json::json!(["spec", "review"])), "loops"),
            "spec,review"
        );

        // The distinction the card is about. `-` is what an empty array renders
        // as everywhere else on this table and reads as "nothing to report",
        // which is exactly the misreading that let a dead node look healthy.
        assert_eq!(super::cell(&node(serde_json::json!([])), "loops"), "NONE");

        // An agent too old to report the field is in the same position as one
        // reporting an empty list: it accepts nothing either way.
        let silent = serde_json::json!({ "capabilities": { "cpus": 8 } });
        assert_eq!(super::cell(&silent, "loops"), "NONE");
    }

    #[test]
    fn distinct_ids_stop_at_the_floor() {
        let ids = [
            "019fcc2e-ccfc-75c2-864e-2aed7bc20318",
            "019fcc2e-7e37-7c80-91b3-77c5e0a14d92",
        ];
        assert_eq!(unique_id_len(&ids), super::SHORT_ID);
    }

    /// The case live data cannot produce on demand but production has 76 of:
    /// two uuidv7s from the same millisecond, identical through the timestamp
    /// AND the twelve random bits that 18 characters reaches.
    #[test]
    fn ids_that_collide_at_the_floor_widen() {
        let ids = [
            "019fcc2e-ccfc-75c2-864e-2aed7bc20318",
            "019fcc2e-ccfc-75c2-91b3-77c5e0a14d92",
        ];
        let n = unique_id_len(&ids);
        assert!(n > super::SHORT_ID, "expected widening, got {n}");
        assert_ne!(ids[0][..n], ids[1][..n], "widened but still ambiguous");
    }

    /// MAIN-627 AC-4: a card in MAIN-331's exact state — a pull request that
    /// conflicts with `main`, at a head every automatic gate had already
    /// concluded on — enqueues a run and says it is a rebase. The `nothing
    /// raised` line was the whole of what a person could get out of that card.
    #[test]
    fn a_forced_conflict_repair_reports_the_rebase_it_raised() {
        let raised = serde_json::json!({
            "raised": [{ "seed": "repair PR #493 — it CONFLICTS with its base branch \
                                  `main` and cannot be merged." }],
            "live": 0, "withheld": 0,
        });
        let line = super::enqueue_report("MAIN-331", &raised);
        assert!(line.contains("raised a build run for MAIN-331"), "{line}");
        assert!(line.contains("REBASE"), "{line}");
        assert!(!line.contains("nothing raised"), "{line}");

        // An ordinary repair still reads as one — the rebase note is not a
        // label every raised run wears.
        let plain = serde_json::json!({
            "raised": [{ "seed": "repair PR #493" }], "live": 0, "withheld": 0,
        });
        let line = super::enqueue_report("MAIN-331", &plain);
        assert!(line.contains("raised a build run for MAIN-331"), "{line}");
        assert!(!line.contains("REBASE"), "{line}");
    }

    #[test]
    fn nothing_raised_and_already_building_stay_distinguishable() {
        let none = serde_json::json!({ "raised": [], "live": 0, "withheld": 0 });
        assert!(super::enqueue_report("MAIN-1", &none).contains("nothing raised"));
        let busy = serde_json::json!({ "raised": [], "live": 1, "withheld": 0 });
        assert!(super::enqueue_report("MAIN-1", &busy).contains("already building"));
    }

    /// Genuinely identical ids cannot be told apart at any width. It must
    /// terminate at the full length rather than loop or panic.
    #[test]
    fn identical_ids_terminate_at_full_length() {
        let ids = ["019fcc2e-ccfc-75c2-864e-2aed7bc20318"; 2];
        assert_eq!(unique_id_len(&ids), ids[0].len());
    }

    #[test]
    fn no_rows_is_not_a_panic() {
        assert_eq!(unique_id_len(&[]), 0);
    }

    /// MAIN-616 AC-4: a node at capacity with nothing actually running is
    /// legible from the CLI. `2/2` alone still reads as a busy machine, so the
    /// paused count is what makes the line answerable.
    #[test]
    fn the_capacity_cell_breaks_the_held_count_down() {
        let cap = serde_json::json!({
            "effective": 2, "source": "operator", "operator": 2, "advertised": null,
            "pinned": false, "held": 2, "held_waiting_on_human": 1
        });
        assert_eq!(
            super::capacity_cell(Some(&cap)),
            "2/2 (operator, 1 waiting on human)"
        );
    }

    /// The ordinary row keeps its shape: a parenthetical saying nobody is
    /// waiting, on every line of a fleet, is noise.
    #[test]
    fn a_busy_node_with_nothing_paused_says_only_what_it_is_holding() {
        let cap = serde_json::json!({
            "effective": 2, "source": "node", "held": 2, "held_waiting_on_human": 0
        });
        assert_eq!(super::capacity_cell(Some(&cap)), "2/2 (node)");
        let idle = serde_json::json!({
            "effective": 4, "source": "default", "held": 0, "held_waiting_on_human": 0
        });
        assert_eq!(super::capacity_cell(Some(&idle)), "0/4 (default)");
    }

    /// A response that never counted renders as it did before MAIN-616 — which
    /// is a different statement from a node holding nothing, and must not be
    /// dressed up as `0/2`.
    #[test]
    fn an_uncounted_capacity_still_renders_its_number() {
        let cap = serde_json::json!({ "effective": 2, "source": "operator" });
        assert_eq!(super::capacity_cell(Some(&cap)), "2 (operator)");
        assert_eq!(super::capacity_cell(None), "-");
        assert_eq!(super::capacity_cell(Some(&serde_json::Value::Null)), "-");
    }
}

/// `nook builds enqueue <KEY>` (MAIN-458 AC-4) — build one card now, through
/// the same convergence the reconciler runs: the CP claims the card, raises a
/// directed run, and dedupes against anything already live for it.
///
/// Naming a card also overrules `blocked` on it (MAIN-489 AC-5) — this is the
/// nudge that brings back a card the loop escalated after three runs concluded
/// nothing, without anybody editing its labels.
pub async fn builds_enqueue(task: &str) -> Result<()> {
    let client = Client::from_config()?;
    let r = client
        .post("/api/v1/builds", serde_json::json!({ "task": task }))
        .await?;
    println!("{}", enqueue_report(task, &r));
    Ok(())
}

/// What the enqueue printed, as a function of the response — so the one line a
/// person reads after asking for a build is testable without a server.
///
/// It says WHICH run it raised, not merely that it did (MAIN-627 AC-4). A
/// conflict repair is the case that needs it: the card looks untouched either
/// way, and "raised a build run" beside a pull request nobody can merge is the
/// same sentence whether the loop understood why or not.
fn enqueue_report(task: &str, r: &serde_json::Value) -> String {
    let raised = r["raised"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    let live = r["live"].as_i64().unwrap_or(0);
    if let Some(job) = raised.first() {
        let seed = job["seed"].as_str().unwrap_or_default();
        let why = if seed.contains("CONFLICTS with its base branch") {
            " — a REBASE: its pull request conflicts with the base branch"
        } else {
            ""
        };
        return format!(
            "{} raised a build run for {task}{why}",
            crate::style::ok_c("✓")
        );
    }
    if live > 0 {
        return "already building — a live run holds this card".into();
    }
    "nothing raised — the card is not currently owed a run \
     (not agent-ready, assigned, held by a recent failed attempt, or \
      already built at this content)"
        .into()
}

/// `nook builds outcome <pr|blocked|nothing> …` (MAIN-459 AC-3) — a build run
/// reports its conclusion, `reviews verdict`'s twin. Job-scoped: reads
/// `NOOK_JOB_ID` from the run's own environment, so an agent cannot conclude a
/// job it is not.
///
/// The control plane records the outcome, mirrors it to the board (comment,
/// column, claim) and validates the PR's `Closes <KEY>` join — opening a PR
/// without reporting it is the silent lie this call ends.
pub async fn builds_outcome(
    conclusion: &str,
    url: Option<&str>,
    question: Option<&str>,
) -> Result<()> {
    let job_id = std::env::var("NOOK_JOB_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .context("NOOK_JOB_ID is not set — this command runs inside a build run")?;
    // The CLI speaks the operator's words; the API records the precise fact.
    let outcome = match conclusion {
        "pr" => "pr_opened",
        "blocked" => "blocked",
        "nothing" => "nothing_to_do",
        other => anyhow::bail!("conclusion must be pr | blocked | nothing, got {other:?}"),
    };
    let mut payload = serde_json::json!({ "outcome": outcome });
    if let Some(u) = url {
        payload["url"] = serde_json::Value::String(u.to_string());
    }
    if let Some(q) = question {
        // `-` reads stdin, the same convention `gh --body-file -` taught.
        let text = if q == "-" {
            use std::io::Read;
            let mut t = String::new();
            std::io::stdin().read_to_string(&mut t)?;
            t
        } else {
            q.to_string()
        };
        payload["question"] = serde_json::Value::String(text);
    }
    let job = Client::from_config()?
        .post(&format!("/api/v1/jobs/{job_id}/outcome"), payload)
        .await?;
    println!(
        "outcome {} recorded",
        crate::style::ok_c(job["build_outcome"].as_str().unwrap_or("?")),
    );
    Ok(())
}

/// The run this process belongs to. Job-scoped commands read it rather than
/// taking an id, which is what stops an agent addressing another run's work.
fn job_of_this_run(verb: &str) -> Result<String> {
    std::env::var("NOOK_JOB_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .with_context(|| {
            format!("NOOK_JOB_ID is not set — `nook emails {verb}` runs inside an investigate run")
        })
}

/// `nook emails read` (MAIN-331 AC-4) — the sealed support message this run was
/// seeded from, decrypted by the control plane and printed here.
///
/// Straight to stdout with no framing, so it can be piped or read as the
/// message it is. Nothing is written to disk: the plaintext exists in this
/// process and in the terminal it prints to, and the run's transcript records
/// tool NAMES rather than their output — so the words stay out of the database
/// unless the agent copies them out itself, which the skill forbids.
pub async fn emails_read() -> Result<()> {
    let job_id = job_of_this_run("read")?;
    let r = Client::from_config()?
        .get(&format!("/api/v1/jobs/{job_id}/email/message"))
        .await?;
    print!("{}", r["message"].as_str().unwrap_or_default());
    Ok(())
}

/// `nook emails record --findings … --draft-reply …` (MAIN-331 AC-2) — the
/// investigation's two halves, onto the chain this run was seeded from.
///
/// One call for both, because they are one report: the control plane refuses a
/// half of it, and this end must not be able to land a half either by making
/// two requests out of one intent.
pub async fn emails_record(findings: &str, draft_reply: &str) -> Result<()> {
    let job_id = job_of_this_run("record")?;
    // `-` reads stdin, the same convention `gh --body-file -` taught — and
    // there is one stdin, so two of them is a mistake worth naming rather than
    // an empty second field the caller discovers on the record.
    if findings == "-" && draft_reply == "-" {
        anyhow::bail!("only one of --findings and --draft-reply can read stdin");
    }
    let read = |v: &str| -> Result<String> {
        if v == "-" {
            let mut t = String::new();
            use std::io::Read;
            std::io::stdin().read_to_string(&mut t)?;
            Ok(t)
        } else {
            Ok(v.to_string())
        }
    };
    let link = Client::from_config()?
        .post(
            &format!("/api/v1/jobs/{job_id}/email/investigation"),
            serde_json::json!({
                "findings": read(findings)?,
                "draft_reply": read(draft_reply)?,
            }),
        )
        .await?;
    println!(
        "{} findings and a sealed draft reply recorded on the chain for {}",
        crate::style::ok_c("✓"),
        link["task_id"].as_str().unwrap_or("this card"),
    );
    Ok(())
}

/// `nook reviews enqueue <workspace>` (MAIN-408 AC-2) — raise a review now.
///
/// Deduped server-side against the sweep by the shared rule, so running this
/// twice is safe: the second call prints the job the first one raised. The CLI
/// deliberately does not decide that itself — a second notion of "already
/// queued" out here is exactly what AC-3 forbids.
pub async fn reviews_enqueue(
    workspace: &str,
    seed: Option<&str>,
    pr: Option<i64>,
    force: bool,
) -> Result<()> {
    let client = Client::from_config()?;
    let mut body = serde_json::json!({ "workspace_id": workspace });
    if let Some(seed) = seed {
        body["seed"] = serde_json::Value::String(seed.to_string());
    }
    if let Some(pr) = pr {
        body["pr"] = serde_json::json!(pr);
    }
    if force {
        body["force"] = serde_json::json!(true);
    }
    let r = client.post("/api/v1/reviews", body).await?;

    let raised = r["raised"].as_array().cloned().unwrap_or_default();
    let live = r["live"].as_u64().unwrap_or(0);
    let withheld = r["withheld"].as_u64().unwrap_or(0);
    for job in &raised {
        println!(
            "raised {} — PR #{}",
            crate::style::ok_c(job["id"].as_str().unwrap_or("?")),
            job["review_pr_number"].as_u64().unwrap_or(0),
        );
    }
    if raised.is_empty() {
        // Zero raised has FOUR causes and they need different fixes, so say
        // which: covered already, held by the ceiling, no forge, or all quiet.
        if live > 0 {
            println!("nothing raised — {live} PR(s) already being reviewed");
        } else if withheld > 0 {
            println!("nothing raised — {withheld} PR(s) owed but held by the review ceiling");
        } else {
            println!(
                "nothing raised — no PR owes a review (quiet repo, all reviewed, or no forge \
                 for this remote)"
            );
        }
    } else if withheld > 0 {
        println!("  +{withheld} more owed, held by the review ceiling");
    }
    Ok(())
}

/// `nook tunnel <port>` (MAIN-404 AC-2) — expose a port on THIS machine.
///
/// The machine is named from `node.toml` rather than asked for: a tunnel is to
/// something running here, and a person who had to type their own node id every
/// time would copy the wrong one eventually. A user token can reach several
/// machines, so it is the CLI's job to say which — a node token is already
/// confined to one and the control plane fills it in.
pub async fn tunnels_open(port: Option<u16>, json: bool) -> Result<()> {
    let Some(port) = port else {
        bail!("which port? `nook tunnel 3000` — or `nook tunnel list` to see what is open");
    };
    let client = Client::from_config()?;
    let mut body = serde_json::json!({ "port": port });
    if let Ok(cfg) = NodeConfig::load() {
        body["node_id"] = Value::String(cfg.node_id);
    }
    // Binds the tunnel's life to this terminal's: exit the session and the
    // tunnel goes with it, rather than outliving what it pointed at.
    if let Some(session) = session_from_env() {
        body["session_id"] = Value::String(session);
    }

    let tunnel = client.post("/api/v1/tunnels", body).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&tunnel)?);
        return Ok(());
    }
    println!(
        "{} {}",
        crate::style::success(&format!("port {port} →")),
        crate::style::bold(tunnel["url"].as_str().unwrap_or_default())
    );
    println!(
        "{}",
        crate::style::dim(
            "anyone in this tenant can open it, signed in; it ends with this session, \
             when it goes idle, or on `nook tunnel stop`"
        )
    );
    Ok(())
}

/// `nook tunnel list` — what is open in this tenant.
pub async fn tunnels_list(json: bool) -> Result<()> {
    let client = Client::from_config()?;
    let tunnels = client.get("/api/v1/tunnels").await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&tunnels)?);
        return Ok(());
    }
    let rows = tunnels.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("{}", crate::style::dim("no tunnels are open"));
        return Ok(());
    }
    for t in &rows {
        println!(
            "{}  {}  {}",
            crate::style::bold(t["label"].as_str().unwrap_or("?")),
            t["url"].as_str().unwrap_or("?"),
            crate::style::dim(&format!(
                "{}:{} · idle {}",
                t["node_name"].as_str().unwrap_or("?"),
                t["port"].as_u64().unwrap_or(0),
                humanize_secs(t["idle_secs"].as_u64().unwrap_or(0)),
            ))
        );
    }
    Ok(())
}

/// `nook tunnel stop [label]` — close one, or everything this session opened.
///
/// The no-argument form is the one people reach for, and it must not be "close
/// everything": a tunnel belongs to whoever opened it, and the session (or,
/// outside one, the machine) is the narrowest honest reading of "mine".
pub async fn tunnels_stop(label: Option<&str>) -> Result<()> {
    let client = Client::from_config()?;
    let labels = match label {
        Some(l) => vec![l.to_string()],
        None => mine(&client).await?,
    };
    if labels.is_empty() {
        println!(
            "{}",
            crate::style::dim("nothing to stop — no tunnel here is yours")
        );
        return Ok(());
    }
    for label in labels {
        client
            .delete(&format!("/api/v1/tunnels/{label}"))
            .await
            .with_context(|| format!("stopping tunnel {label}"))?;
        println!("{} {}", crate::style::ok_c("closed"), label);
    }
    Ok(())
}

/// The labels of the tunnels this session opened — or, with no session, the
/// ones on this machine.
async fn mine(client: &Client) -> Result<Vec<String>> {
    let session = session_from_env();
    let node = NodeConfig::load().ok().map(|c| c.node_id);
    if session.is_none() && node.is_none() {
        bail!("name the tunnel to stop — this is neither a nook session nor a joined machine");
    }
    Ok(client
        .get("/api/v1/tunnels")
        .await?
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|t| match &session {
            Some(s) => t["session_id"].as_str() == Some(s.as_str()),
            None => t["node_id"].as_str() == node.as_deref(),
        })
        .filter_map(|t| t["label"].as_str().map(str::to_string))
        .collect())
}

fn session_from_env() -> Option<String> {
    std::env::var("NOOK_SESSION_ID")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Seconds as something a person reads at a glance. Only ever an age, so the
/// units stop at hours — a tunnel idle for days has been swept.
fn humanize_secs(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

/// `nook epics run <KEY>` (MAIN-144) — enqueue ONE epic-runner pass on the
/// fleet. Manual by design: invocation is authorization, there is no schedule
/// and no auto-feed, and a second enqueue while one runs is refused with the
/// running job's id rather than queued behind it.
pub async fn epics_run(epic: &str, seed: Option<&str>) -> Result<()> {
    let client = Client::from_config()?;
    let mut body = serde_json::json!({ "kind": "epic-run", "target_task_id": epic });
    if let Some(seed) = seed {
        body["seed"] = serde_json::Value::String(seed.to_string());
    }
    let job = client.post("/api/v1/jobs", body).await?;
    println!(
        "epic-run {} — {} (target {})",
        crate::style::ok_c(job["id"].as_str().unwrap_or("?")),
        job["state"].as_str().unwrap_or("?"),
        epic,
    );
    println!("  One pass: merges what the loops' evidence clears, then stops. Watch the job's transcript for the pass report.");
    Ok(())
}

/// `nook tenants export [--out …] [--tenant …]` (MAIN-659) — a tenant's data,
/// leaving.
///
/// Streamed to the file rather than assembled in memory: the archive carries
/// every attachment the tenant ever uploaded, so its size is a property of the
/// tenant and not something this command gets to assume.
///
/// The manifest is read back out of the finished file to print the summary.
/// That is not a detour — it is what proves the thing on disk is a readable
/// archive, which is the only claim worth making after a download.
pub async fn tenants_export(out: Option<&str>, tenant: Option<&str>) -> Result<()> {
    let client = Client::from_config()?;
    let (id, slug, name) = resolve_tenant(&client, tenant).await?;
    let client = client.in_tenant(&id);

    let path = match out {
        Some(p) => std::path::PathBuf::from(p),
        None => std::path::PathBuf::from(format!(
            "{slug}-{}.tar.gz",
            chrono::Utc::now().format("%Y%m%d")
        )),
    };

    let mut file = create_private(&path)?;

    let written = match client
        .stream_to(&format!("/api/v1/tenants/{id}/export"), &mut file)
        .await
    {
        Ok(n) => n,
        Err(e) => {
            // A half-written archive is worse than none: it would make the
            // next attempt refuse to overwrite a file that is garbage.
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
    };
    drop(file);

    let manifest = read_manifest(&path)?;
    let display = path.display().to_string();
    eprintln!(
        "Exported {} ({})",
        crate::style::bold(&name),
        crate::style::dim(&slug)
    );
    if let Some(tables) = manifest["tables"].as_object() {
        for (table, count) in tables {
            eprintln!("  {table:<32} {}", count.as_i64().unwrap_or_default());
        }
    }
    eprintln!(
        "  {:<32} {} ({})",
        "content blobs",
        manifest["blobs"]["count"].as_u64().unwrap_or_default(),
        crate::loop_job::human_bytes(manifest["blobs"]["bytes"].as_u64().unwrap_or_default()),
    );
    // The mode is only claimed where it was actually set.
    let mode = if cfg!(unix) { ", mode 0600" } else { "" };
    eprintln!(
        "  {} {} ({}{mode})",
        crate::style::ok_c("→"),
        display,
        crate::loop_job::human_bytes(written)
    );
    eprintln!(
        "  {}",
        crate::style::dim(
            "Secret values were omitted: git credentials, workspace secrets, vault items \
             and channel tokens are absent from this archive."
        )
    );
    Ok(())
}

/// Which tenant to export: the one named, or the session's own.
///
/// Returns `(id, slug, name)`. The id is what the path needs and the slug is
/// what the default filename needs, so both are resolved once here rather than
/// guessed later.
async fn resolve_tenant(client: &Client, want: Option<&str>) -> Result<(String, String, String)> {
    let list = client.get("/api/v1/tenants").await?;
    let rows = list.as_array().cloned().unwrap_or_default();
    let Some(t) = pick_tenant(&rows, want) else {
        match want {
            Some(w) => bail!("you are not a member of a tenant called '{w}'"),
            None => bail!("this credential belongs to no tenant"),
        }
    };
    Ok((
        t["id"].as_str().unwrap_or_default().to_string(),
        t["slug"].as_str().unwrap_or_default().to_string(),
        t["name"].as_str().unwrap_or_default().to_string(),
    ))
}

/// Create the archive's file, owner-only, refusing to overwrite.
///
/// `create_new` and the mode are both load-bearing. Overwriting would destroy
/// an earlier export on a re-run — the shape of mistake a `--out` typo makes —
/// and the mode is set at CREATION rather than after, because a create-then-
/// chmod leaves a window in which a whole tenant is world-readable.
fn create_private(path: &std::path::Path) -> Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path).with_context(|| {
        format!(
            "cannot create {} — it already exists, or its directory does not",
            path.display()
        )
    })
}

/// Which membership `--tenant` names, or the session's own.
///
/// Slug or id, matched the way the control plane's own tenant header matches
/// them — a person types the slug and a script holds the id, and answering
/// only one of those would make `--tenant` inconsistent with `NOOK_TENANT_ID`.
fn pick_tenant<'a>(rows: &'a [Value], want: Option<&str>) -> Option<&'a Value> {
    match want {
        Some(w) => rows.iter().find(|t| {
            t["slug"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(w))
                || t["id"].as_str().is_some_and(|s| s.eq_ignore_ascii_case(w))
        }),
        // `current` is the tenant this session is scoped to, which is the one
        // every other command in this CLI acts in.
        None => rows
            .iter()
            .find(|t| t["current"].as_bool().unwrap_or(false))
            .or_else(|| rows.first()),
    }
}

/// `manifest.json`, read out of a finished archive.
///
/// It is the archive's first member, so this stops at the first entry rather
/// than walking a file that may be gigabytes long.
fn read_manifest(path: &std::path::Path) -> Result<Value> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("cannot read back {}", path.display()))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut entries = archive
        .entries()
        .context("the downloaded file is not a readable .tar.gz")?;
    let entry = entries
        .next()
        .context("the archive is empty — the export produced nothing")?
        .context("the archive's first entry is unreadable")?;
    serde_json::from_reader(entry).context("the archive's manifest.json is not readable JSON")
}

/// `nook reviews verdict <verdict> [--body …]` (MAIN-455) — a review run
/// reports its conclusion. Job-scoped: reads `NOOK_JOB_ID` from the run's own
/// environment, so an agent cannot verdict a job it is not.
///
/// The control plane posts the comment and labels; this call is the agent's
/// LAST act, replacing the `gh pr comment` / label sequence the skill used to
/// drive by prose.
pub async fn reviews_verdict(verdict: &str, body: Option<&str>) -> Result<()> {
    let job_id = std::env::var("NOOK_JOB_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .context("NOOK_JOB_ID is not set — this command runs inside a review run")?;
    let client = Client::from_config()?;
    let mut payload = serde_json::json!({ "verdict": verdict });
    if let Some(b) = body {
        // `-` reads stdin, the same convention `gh --body-file -` taught.
        let text = if b == "-" {
            use std::io::Read;
            let mut t = String::new();
            std::io::stdin().read_to_string(&mut t)?;
            t
        } else {
            b.to_string()
        };
        payload["body"] = serde_json::Value::String(text);
    }
    let job = client
        .post(&format!("/api/v1/jobs/{job_id}/verdict"), payload)
        .await?;
    println!(
        "verdict {} recorded for PR #{}",
        crate::style::ok_c(job["review_verdict"].as_str().unwrap_or("?")),
        job["review_pr_number"].as_u64().unwrap_or(0),
    );
    Ok(())
}

/// `nook reviews scale <workspace> [n]` (MAIN-445 AC-4) — the CEILING on this
/// repo's review loops, or read the current declaration with no count.
///
/// A ceiling, not a count: the target is `min(open_prs, max)`. No forge exists
/// to count open PRs yet, so the ceiling is currently what runs — which the
/// output says out loud, because "max 2" that always runs 2 would otherwise
/// look like a bug the first time somebody set it on a repo with no PRs.
///
/// The read prints "unset (default 1)" rather than a bare "1" on purpose: those
/// are the same effective number and different facts, and a person checking
/// whether anyone has touched this needs to tell them apart. Printing "1" would
/// send them hunting for a switch nobody ever set.
pub async fn reviews_scale(workspace: &str, count: Option<&str>) -> Result<()> {
    let client = Client::from_config()?;
    let id = resolve_workspace(&client, workspace).await?;
    let path = format!("/api/v1/workspaces/{id}/review-loop");

    let current = match count {
        None => client.get(&path).await?,
        // `unset` is how the third state is reachable from a terminal. Without
        // it a person who set 0 could never get back to "use the default"
        // without knowing what the default is — which is the confusion the
        // NULL/0 split exists to prevent.
        Some("unset") | Some("null") => {
            client
                .put(&path, serde_json::json!({ "max_replicas": null }))
                .await?
        }
        Some(raw) => {
            // Parsed here so a typo is a CLI error naming the argument, rather
            // than a round trip that comes back as a 400 about a JSON field the
            // person never typed.
            let n: u32 = raw.parse().with_context(|| {
                format!("'{raw}' is not a non-negative whole number (or `unset`)")
            })?;
            client
                .put(&path, serde_json::json!({ "max_replicas": n }))
                .await?
        }
    };

    match current["max_replicas"].as_i64() {
        None => println!(
            "review loops: {} — the build's default ceiling",
            crate::style::ok_c("unset (default 1)")
        ),
        Some(0) => println!(
            "review loops: {} — this repo's PRs are not reviewed",
            crate::style::ok_c("0 (off)")
        ),
        Some(n) => println!(
            "review loops: {} — no forge yet, so {n} run regardless of open PRs",
            crate::style::ok_c(&format!("max {n}"))
        ),
    }
    Ok(())
}

/// `nook builds scale <workspace> [n]` (MAIN-461 AC-1) — the CEILING on this
/// repo's build runs, `reviews scale`'s twin, with the same three states: a
/// read prints "unset (default 1)" rather than a bare "1" because those are
/// the same effective number and different facts, `0` is the workspace-level
/// kill-switch, and `unset` is how a terminal reaches the third state back.
pub async fn builds_scale(workspace: &str, count: Option<&str>) -> Result<()> {
    let client = Client::from_config()?;
    let id = resolve_workspace(&client, workspace).await?;
    let path = format!("/api/v1/workspaces/{id}/build-loop");

    let current = match builds_scale_body(count)? {
        None => client.get(&path).await?,
        Some(body) => client.patch(&path, body).await?,
    };

    match current["concurrency"].as_i64() {
        None => println!(
            "build runs: {} — the default ceiling",
            crate::style::ok_c("unset (default 1)")
        ),
        Some(0) => println!(
            "build runs: {} — this repo's cards are not built",
            crate::style::ok_c("0 (off)")
        ),
        Some(n) => println!(
            "build runs: {} in flight at once",
            crate::style::ok_c(&format!("max {n}"))
        ),
    }
    Ok(())
}

/// The PATCH body `builds scale` sends, or `None` for the read a bare
/// `nook builds scale <ws>` is.
///
/// `concurrency` is the whole declaration's name for this column (MAIN-641
/// AC-2), and the body names ONLY it — so scaling a repo cannot disturb its
/// switch or its pin, which is what makes one route safe for both commands.
/// Split out from the IO so the shape of that body is testable without a
/// server, because it is the shape that has to be right.
fn builds_scale_body(count: Option<&str>) -> Result<Option<Value>> {
    Ok(match count {
        None => None,
        Some("unset") | Some("null") => Some(serde_json::json!({ "concurrency": null })),
        Some(raw) => {
            let n: u32 = raw.parse().with_context(|| {
                format!("'{raw}' is not a non-negative whole number (or `unset`)")
            })?;
            Some(serde_json::json!({ "concurrency": n }))
        }
    })
}

/// The PATCH body `builds loop` sends, or `None` when nothing was asked to
/// change and the command is a read.
///
/// Every write is partial: an argument nobody passed is ABSENT from the body,
/// not sent as a null, because the endpoint reads those as "leave it alone" and
/// "clear it" respectively. Typos are refused here so a person hears the
/// argument they typed rather than a 400 about a JSON field they never saw.
fn builds_loop_body(
    state: Option<&str>,
    node: Option<&str>,
    concurrency: Option<&str>,
) -> Result<Option<Value>> {
    let mut body = serde_json::Map::new();
    match state {
        None => {}
        Some("on") => {
            body.insert("enabled".into(), Value::Bool(true));
        }
        Some("off") => {
            body.insert("enabled".into(), Value::Bool(false));
        }
        Some(other) => bail!("'{other}' is not a state — say `on` or `off`"),
    }
    if let Some(n) = node {
        body.insert(
            "node".into(),
            match n {
                "none" | "unset" | "null" => Value::Null,
                name => Value::String(name.to_string()),
            },
        );
    }
    if let Some(c) = concurrency {
        body.insert(
            "concurrency".into(),
            match c {
                "unset" | "null" => Value::Null,
                raw => serde_json::json!(raw.parse::<u32>().with_context(|| format!(
                    "'{raw}' is not a non-negative whole number (or `unset`)"
                ))?),
            },
        );
    }
    Ok((!body.is_empty()).then_some(Value::Object(body)))
}

/// `nook builds loop <workspace> [on|off] [--node …] [--concurrency …]`
/// (MAIN-385 AC-8) — the per-workspace build-loop switch.
///
/// A read with no arguments, because "is this repo building by itself, and
/// where" is the question people ask far more often than they flip it. Every
/// write is partial: `--node azul` on its own moves the pin and says nothing
/// about the switch, which is what the endpoint's absent-means-unchanged rule
/// is for.
pub async fn builds_loop(
    workspace: &str,
    state: Option<&str>,
    node: Option<&str>,
    concurrency: Option<&str>,
) -> Result<()> {
    let body = builds_loop_body(state, node, concurrency)?;
    let client = Client::from_config()?;
    let id = resolve_workspace(&client, workspace).await?;
    let path = format!("/api/v1/workspaces/{id}/build-loop");

    let current = match body {
        None => client.get(&path).await?,
        Some(body) => client.patch(&path, body).await?,
    };

    let on = current["enabled"].as_bool().unwrap_or(false);
    // `null` is unset, and unset is the default ceiling of one — the same
    // reading `/build-loop/status` reports as `desired`.
    let concurrency = current["concurrency"].as_u64().unwrap_or(1);
    if on {
        println!(
            "build loop: {} — up to {concurrency} at once",
            crate::style::ok_c("on")
        );
    } else {
        println!(
            "build loop: {} — this repo's cards are only built when somebody asks",
            crate::style::dim("off")
        );
    }
    match current["node_name"].as_str() {
        Some(name) => println!(
            "  pinned to {} — runs wait for it rather than moving elsewhere",
            crate::style::bold(name)
        ),
        None => println!("  no pinned node — placed on whichever of your nodes is free"),
    }
    Ok(())
}

#[cfg(test)]
mod build_loop_body_tests {
    use super::*;

    /// MAIN-641 AC-6: `scale` writes `{"concurrency": …}` and `unset` still
    /// sends a null. The KEY is what moved — the value semantics are the ones
    /// `nook builds scale` has always had.
    #[test]
    fn scale_names_concurrency_and_keeps_unset_reachable() {
        assert!(
            builds_scale_body(None).expect("read").is_none(),
            "a bare `nook builds scale <ws>` is a read, not a write of nothing"
        );
        assert_eq!(
            builds_scale_body(Some("3")).expect("set"),
            Some(serde_json::json!({ "concurrency": 3 }))
        );
        for spelling in ["unset", "null"] {
            assert_eq!(
                builds_scale_body(Some(spelling)).expect("clear"),
                Some(serde_json::json!({ "concurrency": null })),
                "`{spelling}` is how a terminal reaches the third state back"
            );
        }
        assert!(builds_scale_body(Some("lots")).is_err());
        assert!(builds_scale_body(Some("-1")).is_err());
    }

    /// The absent/null/value distinction the one route rests on: what nobody
    /// typed must not appear in the body at all, or `builds loop --node azul`
    /// would silently unset the ceiling somebody else declared.
    #[test]
    fn loop_sends_only_what_was_typed() {
        assert!(builds_loop_body(None, None, None).expect("read").is_none());
        assert_eq!(
            builds_loop_body(Some("on"), None, None).expect("on"),
            Some(serde_json::json!({ "enabled": true }))
        );
        assert_eq!(
            builds_loop_body(None, Some("azul"), None).expect("pin"),
            Some(serde_json::json!({ "node": "azul" }))
        );
        assert_eq!(
            builds_loop_body(None, Some("none"), None).expect("unpin"),
            Some(serde_json::json!({ "node": null })),
            "`--node none` clears the pin, which absence could never say"
        );
        assert_eq!(
            builds_loop_body(Some("off"), Some("azul"), Some("unset")).expect("all three"),
            Some(serde_json::json!({ "enabled": false, "node": "azul", "concurrency": null }))
        );
        assert!(builds_loop_body(Some("maybe"), None, None).is_err());
        assert!(builds_loop_body(None, None, Some("plenty")).is_err());
    }
}

// ── issues: the board verbs a skill drives a card with (MAIN-138) ────────────

/// `nook issues move <key> <state>` / `--column "<Name>"`.
///
/// Exactly one of the two, refused HERE as well as at the server, because a
/// caller who typed neither should hear the rule rather than spend a round
/// trip to be told it. The server's copy is the one that matters — a hand-built
/// request can still reach it — and both say the same sentence.
pub async fn issues_move(key: &str, state: Option<&str>, column: Option<&str>) -> Result<()> {
    let (body, dest) = match (state, column) {
        (Some(t), None) => (serde_json::json!({ "column_type": t }), t.to_string()),
        (None, Some(name)) => (serde_json::json!({ "column": name }), format!("\"{name}\"")),
        _ => bail!(
            "give exactly one of <state> (backlog|unstarted|started|review|completed|canceled) \
             or --column \"<exact name>\""
        ),
    };
    let client = Client::from_config()?;
    client
        .post(&format!("/api/v1/tasks/{key}/move"), body)
        .await?;
    println!(
        "{} moved {} to {dest}",
        crate::style::ok_c("✓"),
        crate::style::bold(key)
    );
    Ok(())
}

/// `nook issues release <key>` — hand a claimed card back to the queue.
pub async fn issues_release(key: &str) -> Result<()> {
    let client = Client::from_config()?;
    client
        .post(
            &format!("/api/v1/tasks/{key}/release"),
            serde_json::json!({}),
        )
        .await?;
    println!(
        "{} released {} — it is pickable again",
        crate::style::ok_c("✓"),
        crate::style::bold(key)
    );
    Ok(())
}

/// `nook issues prune-worktree <key>` — drop the checkout a finished card made.
pub async fn issues_prune_worktree(key: &str) -> Result<()> {
    let client = Client::from_config()?;
    client
        .post(
            &format!("/api/v1/tasks/{key}/prune-worktree"),
            serde_json::json!({}),
        )
        .await?;
    println!(
        "{} pruned {}'s worktree",
        crate::style::ok_c("✓"),
        crate::style::bold(key)
    );
    Ok(())
}

/// `nook issues set-parent <key> <EPIC|none>` — re-file under an epic, or detach.
///
/// `none` is the literal a shell can type; the wire form is a JSON `null`, and
/// the tri-state PATCH field is what distinguishes it from "leave it alone".
pub async fn issues_set_parent(key: &str, parent: &str) -> Result<()> {
    let detach = parent.eq_ignore_ascii_case("none");
    let body = serde_json::json!({
        "parent": if detach { Value::Null } else { Value::String(parent.to_string()) }
    });
    let client = Client::from_config()?;
    let (status, resp) = client
        .patch_status(&format!("/api/v1/tasks/{key}"), body)
        .await?;
    if !(200..300).contains(&status) {
        let msg = resp
            .get("error")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| resp.to_string());
        bail!("{status} /api/v1/tasks/{key}: {msg}");
    }
    if detach {
        println!(
            "{} detached {} from its epic",
            crate::style::ok_c("✓"),
            crate::style::bold(key)
        );
    } else {
        println!(
            "{} filed {} under {parent}",
            crate::style::ok_c("✓"),
            crate::style::bold(key)
        );
    }
    Ok(())
}

/// The wire format `nook issues attach` sends, and the sentence it prints
/// when the server refuses (MAIN-594).
#[cfg(test)]
mod upload_wire {
    use super::*;

    fn text(body: &[u8]) -> String {
        String::from_utf8_lossy(body).to_string()
    }

    /// AC-2: the upload route SKIPS a field with no filename, so a part missing
    /// one uploads nothing at all — and the type has to be the one we chose,
    /// not whatever a default would be.
    #[test]
    fn the_part_carries_a_filename_and_the_chosen_type() {
        let body = multipart_body(
            "B0UND",
            "run.webm",
            "video/webm",
            b"\x1a\x45\xdf\xa3".to_vec(),
        );
        let head = text(&body);
        assert!(
            head.starts_with(
                "--B0UND\r\nContent-Disposition: form-data; name=\"file\"; \
                              filename=\"run.webm\"\r\nContent-Type: video/webm\r\n\r\n"
            ),
            "{head}"
        );
        assert!(head.ends_with("\r\n--B0UND--\r\n"), "{head}");
    }

    /// Binary is what this carries — a video, a screenshot — so the bytes have
    /// to arrive exactly as they left, with nothing re-encoded around them.
    #[test]
    fn the_bytes_survive_byte_for_byte() {
        let bytes: Vec<u8> = (0u8..=255).collect();
        let body = multipart_body(
            "B0UND",
            "blob.bin",
            "application/octet-stream",
            bytes.clone(),
        );
        let start = text(&body).find("\r\n\r\n").expect("a header break") + 4;
        assert_eq!(
            &body[start..body.len() - "\r\n--B0UND--\r\n".len()],
            &bytes[..]
        );
    }

    /// A quote or a newline in the name would close the header early and let a
    /// filename write the rest of the part.
    #[test]
    fn a_filename_cannot_escape_its_header() {
        let body = multipart_body("B0UND", "a\"b\r\nc.png", "image/png", vec![]);
        let head = text(&body);
        assert!(head.contains("filename=\"a_b__c.png\""), "{head}");
        assert_eq!(head.matches("Content-Type:").count(), 1, "{head}");
    }

    /// AC-5: the cap is the server's, so its refusal is the only account of it
    /// that can be right — reported verbatim rather than re-worded here.
    #[test]
    fn a_refusal_is_reported_in_the_servers_own_words() {
        assert_eq!(
            api_message(
                413,
                "/api/v1/user-content",
                r#"{"error":"that file is larger than the 25 MiB upload limit"}"#
            ),
            "that file is larger than the 25 MiB upload limit"
        );
        // A body that is not one of ours still says what happened.
        let fallback = api_message(502, "/api/v1/user-content", "<html>bad gateway</html>");
        assert!(
            fallback.contains("502") && fallback.contains("bad gateway"),
            "{fallback}"
        );
    }
}

/// The `SANDBOX` column (MAIN-643 AC-6). A refusal that does not say WHY sends
/// every operator to a shell on the box to find out, which is the trip this
/// column exists to save.
#[cfg(test)]
mod sandbox_column {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_failure_renders_the_reason_it_needs() {
        for (reason, want) in [
            ("no_docker", "NO (no docker)"),
            ("not_published", "NO (not published)"),
            ("no_credentials", "NO (no credentials)"),
            ("pull_refused", "NO (pull refused)"),
            ("not_present", "NO (image absent)"),
        ] {
            let v = json!({"state": "unavailable", "detail": "…", "reason": reason});
            assert_eq!(sandbox_cell(Some(&v)), want);
        }
    }

    /// A report from an agent that predates the field still renders — as a
    /// refusal, because that is what it said, with the reason unknown.
    #[test]
    fn an_older_report_without_a_reason_still_refuses() {
        let v = json!({"state": "unavailable", "detail": "no image"});
        assert_eq!(sandbox_cell(Some(&v)), "NO (unknown)");
    }

    /// AC-4 reaches the column too: a node three minutes into a pull is not
    /// rendered as one somebody has to go and fix.
    #[test]
    fn a_warming_node_is_not_rendered_as_a_broken_one() {
        let pulling =
            json!({"state": "pulling", "image": "ghcr.io/nook-os/nook-job-sandbox:1.2.3"});
        assert_eq!(sandbox_cell(Some(&pulling)), "pulling");

        let ready = json!({"state": "ready", "image": "img (unprivileged)"});
        assert_eq!(sandbox_cell(Some(&ready)), "yes (img (unprivileged))");
        assert_eq!(sandbox_cell(None), "-");
    }
}

#[cfg(test)]
mod tenant_export_tests {
    use super::pick_tenant;
    use serde_json::json;

    fn memberships() -> Vec<serde_json::Value> {
        vec![
            json!({"id": "11111111-1111-1111-1111-111111111111", "slug": "acme", "name": "Acme", "current": false}),
            json!({"id": "22222222-2222-2222-2222-222222222222", "slug": "Hein", "name": "Hein", "current": true}),
        ]
    }

    /// With no `--tenant`, the export follows the session, exactly as every
    /// other command in this CLI does.
    #[test]
    fn the_default_is_the_session_s_tenant() {
        let rows = memberships();
        assert_eq!(pick_tenant(&rows, None).unwrap()["slug"], "Hein");
    }

    /// A slug or an id, either case — a person types one and a script holds
    /// the other.
    #[test]
    fn a_named_tenant_is_matched_by_slug_or_id() {
        let rows = memberships();
        assert_eq!(pick_tenant(&rows, Some("acme")).unwrap()["name"], "Acme");
        assert_eq!(pick_tenant(&rows, Some("ACME")).unwrap()["name"], "Acme");
        assert_eq!(pick_tenant(&rows, Some("hein")).unwrap()["name"], "Hein");
        assert_eq!(
            pick_tenant(&rows, Some("11111111-1111-1111-1111-111111111111")).unwrap()["name"],
            "Acme"
        );
    }

    /// A tenant you are not a member of is not found here rather than refused
    /// by the server, so the message names what you asked for.
    #[test]
    fn a_tenant_you_do_not_belong_to_is_not_picked() {
        assert!(pick_tenant(&memberships(), Some("someone-else")).is_none());
        assert!(pick_tenant(&[], None).is_none());
    }

    /// AC-8: the archive is owner-only from the moment it exists, and a second
    /// export to the same path is refused rather than destroying the first.
    #[test]
    fn the_archive_is_private_and_never_overwritten() {
        use super::create_private;

        let dir =
            std::env::temp_dir().join(format!("nook-export-cli-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("t.tar.gz");

        create_private(&path).expect("the first write creates the file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "an export is not world-readable");
        }
        let again = create_private(&path);
        assert!(
            again.is_err(),
            "a second export to the same path must refuse, not overwrite"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
