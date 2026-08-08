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
/// so an agent running there is already scoped: `nook create task` resolves one
/// board instead of asking which. It is per-session and disappears with the
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

    pub async fn delete(&self, path: &str) -> Result<Value> {
        self.send(reqwest::Method::DELETE, path, None).await
    }

    /// PUT is the idempotent-write verb the board uses for labels: "make this
    /// true", safe to repeat, which is what a retrying agent needs.
    pub async fn put(&self, path: &str, body: Value) -> Result<Value> {
        self.send(reqwest::Method::PUT, path, Some(body)).await
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
fn pick_one(rows: Vec<Value>, want: &str, keys: &[&str], resource: &str) -> Result<Value> {
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
    let workspaces = client.get("/api/v1/workspaces").await?;
    let ws = workspaces
        .as_array()
        .cloned()
        .unwrap_or_default()
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

    // Secrets live under a workspace; everything else is a flat collection.
    let value = if resource == "secrets" {
        secrets_across_workspaces(&client, name).await?
    } else {
        client.get(&format!("/api/v1/{resource}")).await?
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

/// Secrets are per-workspace; list them all with their workspace for context.
async fn secrets_across_workspaces(client: &Client, workspace: Option<&str>) -> Result<Value> {
    let workspaces = client.get("/api/v1/workspaces").await?;
    let mut out = Vec::new();
    for ws in workspaces.as_array().cloned().unwrap_or_default() {
        let (Some(id), Some(name)) = (
            ws.get("id").and_then(Value::as_str),
            ws.get("name").and_then(Value::as_str),
        ) else {
            continue;
        };
        if let Some(want) = workspace {
            let slug = ws.get("slug").and_then(Value::as_str).unwrap_or_default();
            if !name.eq_ignore_ascii_case(want) && !slug.eq_ignore_ascii_case(want) {
                continue;
            }
        }
        let secrets = client
            .get(&format!("/api/v1/workspaces/{id}/secrets"))
            .await
            .unwrap_or(Value::Null);
        for s in secrets.as_array().cloned().unwrap_or_default() {
            let mut row = s.clone();
            if let Some(obj) = row.as_object_mut() {
                obj.insert("workspace".into(), Value::String(name.to_string()));
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
        // One tenant failing must not lose the others: a membership can be
        // revoked between the list and the fetch, and a partial answer that
        // says so beats no answer at all.
        let rows = match scoped.get(&format!("/api/v1/{resource}")).await {
            Ok(v) => v.as_array().cloned().unwrap_or_default(),
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
        "secrets" => vec!["workspace", "name", "updated_at"],
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
    let mut node = row;
    for part in key.split('.') {
        match node.get(part) {
            Some(v) => node = v,
            None => return "-".into(),
        }
    }
    render_value(key, node)
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

fn print_table(resource: &str, rows: &[Value]) {
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

// ── migrating checkouts into the per-control-plane root (MAIN-107) ───────────
//
// MAIN-58 gave new enrollments a per-control-plane slugged root
// (`~/.nook/workspace/<cp-slug>/…`); nodes that joined earlier still hold their
// checkouts in the flat legacy root. This verb relocates them, coordinating the
// on-disk move with a control-plane path rewrite so the checkouts keep their
// identity instead of looking like a delete-everything-then-rediscover.

/// A checkout as discovery sees it: its path and whether it is a linked
/// worktree (a `.git` file) rather than a primary (a `.git` directory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    pub path: std::path::PathBuf,
    pub is_worktree: bool,
}

/// One planned directory move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMove {
    pub old: std::path::PathBuf,
    pub new: std::path::PathBuf,
    pub is_worktree: bool,
}

/// Compute the migration plan (MAIN-107 AC-1). Pure, so it is unit-testable
/// without touching disk or the network.
///
/// Every checkout under `legacy_root` maps to the SAME relative path under
/// `slugged_root`. A checkout already under `slugged_root` yields no move,
/// which is what makes an already-migrated node produce an empty plan —
/// idempotent by construction. Because the slugged root is nested inside the
/// legacy root in the default layout (`~/.nook/workspace/<slug>` under
/// `~/.nook/workspace`), the "already under the slug" test is applied FIRST;
/// otherwise a checkout that had already moved would still match `legacy_root`
/// and be re-nested a second level deep.
pub fn migration_plan(
    checkouts: &[Checkout],
    legacy_root: &std::path::Path,
    slugged_root: &std::path::Path,
) -> Vec<PlannedMove> {
    let mut moves = Vec::new();
    for c in checkouts {
        if c.path.starts_with(slugged_root) {
            continue; // already migrated
        }
        let Ok(rel) = c.path.strip_prefix(legacy_root) else {
            continue; // not under the legacy root — not ours to move
        };
        if rel.as_os_str().is_empty() {
            continue; // the root itself is not a checkout to relocate
        }
        moves.push(PlannedMove {
            old: c.path.clone(),
            new: slugged_root.join(rel),
            is_worktree: c.is_worktree,
        });
    }
    moves
}

/// Are `old` and the eventual location of `new` on the same filesystem?
///
/// `fs::rename` cannot cross filesystems (EXDEV), and the ticket wants no
/// partial migrations — so every move is checked BEFORE any is performed. The
/// destination usually does not exist yet, so its device id is read from the
/// nearest existing ancestor (the slugged root's parent, in the default case).
#[cfg(unix)]
fn same_filesystem(old: &std::path::Path, new: &std::path::Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let old_dev = std::fs::metadata(old)?.dev();
    let mut anc = new;
    let dest = loop {
        if anc.exists() {
            break anc;
        }
        match anc.parent() {
            Some(p) => anc = p,
            None => break anc,
        }
    };
    let new_dev = std::fs::metadata(dest)?.dev();
    Ok(old_dev == new_dev)
}

#[cfg(not(unix))]
fn same_filesystem(_old: &std::path::Path, _new: &std::path::Path) -> std::io::Result<bool> {
    // Nodes run on Unix; on anything else the rename itself surfaces EXDEV.
    Ok(true)
}

/// The PID of a live `nook run` agent on this machine, if any (MAIN-107 AC-2).
///
/// Stale-PID tolerant: a pidfile left by a crashed agent names a PID that is no
/// longer alive — or one an unrelated program has since been assigned — and
/// must not block a migration. So the PID must both be alive AND look like a
/// nook process before it counts as "the agent is running".
fn running_agent() -> Option<u32> {
    let path = crate::config::pidfile_path().ok()?;
    let pid: u32 = std::fs::read_to_string(&path).ok()?.trim().parse().ok()?;
    if pid == std::process::id() {
        return None;
    }
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new();
    let spid = Pid::from_u32(pid);
    sys.refresh_processes(ProcessesToUpdate::Some(&[spid]), true);
    let proc = sys.process(spid)?;
    let mentions_nook = |s: &std::ffi::OsStr| s.to_string_lossy().contains("nook");
    let looks_like_nook = mentions_nook(proc.name())
        || proc
            .exe()
            .map(|e| mentions_nook(e.as_os_str()))
            .unwrap_or(false)
        || proc.cmd().iter().any(|a| mentions_nook(a));
    looks_like_nook.then_some(pid)
}

/// Repair the primary↔worktree `.git` links for every moved worktree
/// (MAIN-107 AC-3). Worktrees are the depth-2 siblings of their primary
/// (`owner/repo` and `owner/repo__branch`, per `gitops::add_worktree`), so each
/// moved worktree is grouped under the moved primary it belongs to and repaired
/// from that primary's NEW location with the worktree's NEW path.
fn repair_moved_worktrees(plan: &[PlannedMove]) {
    for primary in plan.iter().filter(|m| !m.is_worktree) {
        let base = primary
            .old
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let prefix = format!("{base}__");
        let parent = primary.old.parent();
        let worktrees: Vec<std::path::PathBuf> = plan
            .iter()
            .filter(|m| m.is_worktree && m.old.parent() == parent)
            .filter(|m| {
                m.old
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix))
            })
            .map(|m| m.new.clone())
            .collect();
        if let Err(e) = crate::gitops::repair_worktrees(&primary.new, &worktrees) {
            eprintln!(
                "! git worktree repair failed for {}: {e}",
                primary.new.display()
            );
        }
    }
}

/// `nook migrate-workspaces [--apply]` — relocate this machine's checkouts into
/// the per-control-plane slugged root (MAIN-107).
pub async fn migrate_workspaces(apply: bool) -> Result<()> {
    let cfg = NodeConfig::load().context("run `nook setup` first")?;

    // The target is this control plane's per-node slugged root; the legacy root
    // is the flat default it replaced — the slugged root's own parent.
    let slugged = std::path::PathBuf::from(crate::config::expand_path(
        &crate::config::default_workspace_root(cfg.tenant_slug.as_deref(), &cfg.server),
    ));
    let legacy = slugged
        .parent()
        .map(std::path::Path::to_path_buf)
        .context("cannot determine the legacy workspace root")?;

    // Scan every place a checkout could be: the configured roots plus the legacy
    // and slugged roots explicitly, so the plan is complete whether node.toml
    // still names the flat root or has already been pointed at the slug.
    let mut scan_roots: Vec<String> = cfg.workspace_roots.clone();
    scan_roots.push(legacy.to_string_lossy().to_string());
    scan_roots.push(slugged.to_string_lossy().to_string());
    let discovered = crate::discovery::scan(&scan_roots);

    // Discovery already knows what a checkout is (and which are worktrees);
    // enumerate through it so the plan covers exactly what the control plane
    // reconciles — including sibling worktrees.
    let mut seen = std::collections::HashSet::new();
    let checkouts: Vec<Checkout> = discovered
        .into_iter()
        .filter(|w| seen.insert(w.path.clone()))
        .map(|w| Checkout {
            path: std::path::PathBuf::from(w.path),
            is_worktree: w.worktree,
        })
        .collect();

    let plan = migration_plan(&checkouts, &legacy, &slugged);

    if plan.is_empty() {
        println!(
            "✓ Already under {} — nothing to migrate.",
            slugged.display()
        );
        return Ok(());
    }

    // The plan is printed either way — dry-run is the whole point of the default.
    println!("Migration plan — {} checkout(s):", plan.len());
    println!("  legacy root:  {}", legacy.display());
    println!("  slugged root: {}", slugged.display());
    println!();
    for m in &plan {
        let kind = if m.is_worktree {
            "worktree"
        } else {
            "primary "
        };
        println!("  {kind}  {}", m.old.display());
        println!("             → {}", m.new.display());
    }
    println!();

    if !apply {
        println!("Dry run — nothing moved. Re-run with --apply to perform the migration.");
        return Ok(());
    }

    // AC-2: never move under a live agent. Its periodic discovery, taken
    // mid-move, would report a half-emptied tree and the reconcile would DELETE
    // the checkout rows it can no longer see.
    if let Some(pid) = running_agent() {
        bail!(
            "the node agent is running (pid {pid}) — refusing to migrate.\n\n\
             Stop it, then re-run `nook migrate-workspaces --apply`:\n\
             • systemd (user):   systemctl --user stop nook-node\n\
             • systemd (system): sudo systemctl stop nook-node\n\
             • docker:           docker compose stop node\n\
             • manual:           stop the `nook run` process\n\n\
             Why: a running agent re-scans on a timer, and a scan taken mid-move \
             reports a half-emptied tree — which the control plane reconciles by \
             deleting the checkout rows it can no longer see."
        );
    }

    // AC-3: EXDEV pre-check. Verify every move is same-filesystem BEFORE moving
    // anything, so a cross-device layout aborts cleanly with zero partial moves.
    let mut cross = Vec::new();
    for m in &plan {
        match same_filesystem(&m.old, &m.new) {
            Ok(true) => {}
            Ok(false) => cross.push(m.old.clone()),
            Err(e) => bail!(
                "cannot stat {} for a same-filesystem check: {e}",
                m.old.display()
            ),
        }
    }
    if !cross.is_empty() {
        let list = cross
            .iter()
            .map(|p| format!("  {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "these checkouts are on a different filesystem than {} and cannot be \
             relocated by rename:\n{list}\n\n\
             This migration does not cross filesystems, and aborts before moving \
             anything so there are no partial migrations. Symlinked / cross-device \
             checkouts stay `nook import`'s job.",
            slugged.display()
        );
    }

    // Perform the moves. Same-filesystem renames leave the working trees
    // in place byte-for-byte; only the `.git` links need repairing afterward.
    for m in &plan {
        if let Some(parent) = m.new.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        std::fs::rename(&m.old, &m.new)
            .with_context(|| format!("cannot move {} → {}", m.old.display(), m.new.display()))?;
        println!("✓ moved {} → {}", m.old.display(), m.new.display());
    }

    // AC-3: repair every moved worktree so discovery still sees a valid `.git`.
    repair_moved_worktrees(&plan);

    // AC-4: hand the control plane the old→new pairs so it rewrites its two
    // durable path records in place — no destructive reconcile, no row churn,
    // no re-delivered `.env`, no stale `worktree_path`.
    let pairs: Vec<Value> = plan
        .iter()
        .map(|m| {
            serde_json::json!({
                "old": m.old.to_string_lossy(),
                "new": m.new.to_string_lossy(),
            })
        })
        .collect();
    let client = Client::from_config()?;
    let resp = client
        .post(
            &format!("/api/v1/nodes/{}/migrate-paths", cfg.node_id),
            serde_json::json!({ "pairs": pairs }),
        )
        .await
        .context(
            "the on-disk moves succeeded but the control-plane path rewrite failed \
             — start the agent and it will reconcile, though a task's worktree_path \
             may need re-setting",
        )?;
    let nw = resp
        .get("node_workspaces_updated")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let tk = resp
        .get("tasks_updated")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    println!("✓ control plane rewrote {nw} checkout record(s) and {tk} task worktree path(s)");

    // AC-5: point node.toml at the slugged root so the next connect converges.
    // The flat legacy root (and any pre-existing slug entry) becomes the slug;
    // a custom explicit root is left exactly as it is.
    let slugged_cfg =
        crate::config::default_workspace_root(cfg.tenant_slug.as_deref(), &cfg.server);
    let mut roots: Vec<String> = Vec::new();
    for r in &cfg.workspace_roots {
        let expanded = std::path::PathBuf::from(crate::config::expand_path(r));
        if expanded == legacy || expanded == slugged {
            if !roots.contains(&slugged_cfg) {
                roots.push(slugged_cfg.clone());
            }
        } else if !roots.contains(r) {
            roots.push(r.clone());
        }
    }
    if !roots.contains(&slugged_cfg) {
        roots.push(slugged_cfg.clone());
    }
    let mut new_cfg = cfg.clone();
    new_cfg.workspace_roots = roots;
    new_cfg.save()?;
    println!("✓ node.toml workspace root is now {slugged_cfg}");

    // AC-5 + AC-6.
    println!();
    println!("Migration complete. Start the agent again:");
    println!("  nook run   (or: systemctl --user start nook-node / docker compose start node)");
    println!("The next connect's discovery converges with zero row churn.");
    println!();
    println!(
        "Note: stopping running sessions was only to pause the agent's discovery, \
         not because the shells break. The moves were same-filesystem renames, so \
         any running tmux panes keep working and report the new path afterward \
         (sessions store no cwd server-side)."
    );
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
    let list = client.get(&format!("/api/v1/{resource}")).await?;
    let rows = list.as_array().cloned().unwrap_or_default();

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
    let workspaces = client.get("/api/v1/workspaces").await?;
    let ws = pick_one(
        workspaces.as_array().cloned().unwrap_or_default(),
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

/// `nook tasks` — the pick query from a terminal.
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
    // and `nook tasks` cheerfully returned another workspace's cards.
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
    let list = client.get("/api/v1/workspaces").await?;
    list.as_array()
        .into_iter()
        .flatten()
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
            .with_context(|| format!("no board '{n}' — try `nook tasks` or omit --board")),
    }
}

/// The flags for `nook create task`, one field per flag (mirrors `main.rs`).
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

/// `nook create task --title …` — file a task on the board (MAIN-89 AC-3).
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
    // `nook tasks` — so a filer inside a repo files against that repo by default.
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

/// `nook relate <BLOCKER> <kind> <DEPENDENT>` (MAIN-89 AC-4).
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
    q
}

// One parameter per CLI flag by design — this is the dispatch seam for
// `nook tasks`, and a struct would just move the same list one hop away.
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

/// `nook task <key>` — one whole issue, the way an agent reads it.
pub async fn task(key: &str, json: bool) -> Result<()> {
    let client = Client::from_config()?;
    let resp = client.get(&format!("/api/v1/tasks/{key}")).await?;
    if json {
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
        for c in comments {
            println!(
                "\n{} {}",
                crate::style::bold(c["author_name"].as_str().unwrap_or("?")),
                crate::style::dim(c["created_at"].as_str().unwrap_or("")),
            );
            println!("{}", c["body_md"].as_str().unwrap_or(""));
        }
    }
    Ok(())
}

/// `nook comment <key> <body>` — where the reasoning goes.
pub async fn comment(key: &str, body: &str) -> Result<()> {
    let client = Client::from_config()?;
    let host = sysinfo::System::host_name().unwrap_or_else(|| "unknown".into());
    client
        .post(
            &format!("/api/v1/tasks/{key}/comments"),
            serde_json::json!({
                "body_md": body,
                "author_name": format!("nook cli on {host}"),
            }),
        )
        .await?;
    println!(
        "{} commented on {}",
        crate::style::ok_c("✓"),
        crate::style::bold(key)
    );
    Ok(())
}

/// `nook set-description <key> <body>` — replace a task's description safely.
///
/// Read the current version, PATCH with the optimistic-concurrency guard, and
/// on a 409 (someone else edited it meanwhile) re-read and retry a bounded
/// number of times. If it keeps conflicting, exit non-zero rather than silently
/// losing the edit (AC-4).
pub async fn set_description(key: &str, description: &str) -> Result<()> {
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

/// `nook label <key> <name> [--remove]`.
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

/// `nook claim <key>` — take the work.
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
    /// running `nook claim` by hand is never blocked.
    #[test]
    fn no_session_workspace_never_blocks() {
        assert!(!claim_blocked(None, Some(OTHER), false));
        assert!(!claim_blocked(None, None, false));
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
    }

    #[test]
    fn tasks_query_omits_unset_filters() {
        // No workspace, no type, no parent → none of those keys appear, so a
        // bare `nook tasks` hits `/api/v1/tasks` with nothing extra.
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

#[cfg(test)]
mod migrate_tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn co(path: &str, is_worktree: bool) -> Checkout {
        Checkout {
            path: PathBuf::from(path),
            is_worktree,
        }
    }

    /// A flat legacy root: `<legacy>/repo` → `<slug>/repo`.
    #[test]
    fn flat_legacy_root_maps_by_relative_path() {
        let legacy = Path::new("/home/u/.nook/workspace");
        let slug = Path::new("/home/u/.nook/workspace/cp.example.com");
        let plan = migration_plan(
            &[co("/home/u/.nook/workspace/widgets", false)],
            legacy,
            slug,
        );
        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan[0].new,
            PathBuf::from("/home/u/.nook/workspace/cp.example.com/widgets")
        );
        assert!(!plan[0].is_worktree);
    }

    /// A nested `owner/repo` layout keeps the whole relative path.
    #[test]
    fn nested_org_repo_layout_is_preserved() {
        let legacy = Path::new("/w");
        let slug = Path::new("/w/cp");
        let plan = migration_plan(&[co("/w/acme/services", false)], legacy, slug);
        assert_eq!(plan[0].new, PathBuf::from("/w/cp/acme/services"));
    }

    /// A primary and its sibling worktree both move, and the worktree flag rides
    /// through so the repair step can find it.
    #[test]
    fn worktree_siblings_are_included() {
        let legacy = Path::new("/w");
        let slug = Path::new("/w/cp");
        let plan = migration_plan(
            &[co("/w/acme/repo", false), co("/w/acme/repo__feature", true)],
            legacy,
            slug,
        );
        assert_eq!(plan.len(), 2);
        let wt = plan.iter().find(|m| m.is_worktree).expect("worktree move");
        assert_eq!(wt.old, PathBuf::from("/w/acme/repo__feature"));
        assert_eq!(wt.new, PathBuf::from("/w/cp/acme/repo__feature"));
    }

    /// A node already fully under the slugged root produces an EMPTY plan — even
    /// though the slug is nested inside the legacy root, the "already migrated"
    /// test wins so nothing is re-nested. This is the idempotency guarantee.
    #[test]
    fn already_migrated_is_an_empty_plan() {
        let legacy = Path::new("/w");
        let slug = Path::new("/w/cp");
        let plan = migration_plan(
            &[
                co("/w/cp/acme/repo", false),
                co("/w/cp/acme/repo__wip", true),
            ],
            legacy,
            slug,
        );
        assert!(
            plan.is_empty(),
            "already-migrated checkouts must not move: {plan:?}"
        );
    }

    /// A mixed tree — some moved already, some not — plans only the stragglers.
    #[test]
    fn mixed_tree_plans_only_the_unmigrated() {
        let legacy = Path::new("/w");
        let slug = Path::new("/w/cp");
        let plan = migration_plan(
            &[co("/w/cp/acme/done", false), co("/w/acme/todo", false)],
            legacy,
            slug,
        );
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].old, PathBuf::from("/w/acme/todo"));
    }

    /// The same-filesystem check passes for two paths under one tmpdir, and
    /// still resolves when the destination does not exist yet (it reads the
    /// nearest existing ancestor). A genuine cross-device abort needs a second
    /// mount and so is exercised by hand, but the "no partial moves" ordering is
    /// asserted here: the pre-check runs over the whole plan before any rename.
    #[cfg(unix)]
    #[test]
    fn same_filesystem_holds_within_one_tmpdir() {
        let base = unique_tmp("nook-fs");
        let old = base.join("a");
        std::fs::create_dir_all(&old).unwrap();
        let new = base.join("sub/does/not/exist/yet/b");
        assert!(same_filesystem(&old, &new).unwrap());
        let _ = std::fs::remove_dir_all(&base);
    }

    fn git_available() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    fn unique_tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn git(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git runs")
    }

    /// End to end on a real repo + worktree: build a legacy layout, plan it,
    /// move the directories, repair, and assert the moved worktree is a valid
    /// repo with a clean `git status`. Gated on `git` being installed.
    #[cfg(unix)]
    #[test]
    fn worktree_repair_keeps_a_moved_worktree_valid() {
        if !git_available() {
            return;
        }
        let root = unique_tmp("nook-mig");
        let legacy = root.join("workspace");
        let slug = legacy.join("cp.example.com");
        let primary = legacy.join("acme/repo");
        std::fs::create_dir_all(&primary).unwrap();

        assert!(git(&primary, &["init", "-q"]).status.success());
        git(&primary, &["config", "user.email", "t@example.com"]);
        git(&primary, &["config", "user.name", "t"]);
        std::fs::write(primary.join("README.md"), "hi").unwrap();
        git(&primary, &["add", "."]);
        assert!(git(&primary, &["commit", "-q", "-m", "init"])
            .status
            .success());
        let wt = legacy.join("acme/repo__feature");
        assert!(git(
            &primary,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "feature"
            ],
        )
        .status
        .success());

        // Enumerate exactly as discovery does, then plan.
        let discovered = crate::discovery::scan(&[legacy.to_string_lossy().to_string()]);
        let checkouts: Vec<Checkout> = discovered
            .into_iter()
            .map(|w| Checkout {
                path: PathBuf::from(w.path),
                is_worktree: w.worktree,
            })
            .collect();
        let plan = migration_plan(&checkouts, &legacy, &slug);
        assert_eq!(plan.len(), 2, "primary + worktree: {plan:?}");

        for m in &plan {
            std::fs::create_dir_all(m.new.parent().unwrap()).unwrap();
            std::fs::rename(&m.old, &m.new).unwrap();
        }
        // Before repair, the moved worktree's .git link is stale.
        repair_moved_worktrees(&plan);

        let moved_wt = slug.join("acme/repo__feature");
        let moved_primary = slug.join("acme/repo");
        let status = git(&moved_wt, &["status", "--porcelain"]);
        assert!(
            status.status.success(),
            "moved worktree must be a valid repo after repair: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        assert!(
            status.stdout.is_empty(),
            "moved worktree status must be clean: {}",
            String::from_utf8_lossy(&status.stdout)
        );
        let listed = git(&moved_primary, &["worktree", "list"]);
        assert!(
            String::from_utf8_lossy(&listed.stdout).contains(moved_wt.to_str().unwrap()),
            "primary must list the worktree at its new path"
        );

        let _ = std::fs::remove_dir_all(&root);
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
}

/// `nook reviews enqueue <workspace>` (MAIN-408 AC-2) — raise a review now.
///
/// Deduped server-side against the sweep by the shared rule, so running this
/// twice is safe: the second call prints the job the first one raised. The CLI
/// deliberately does not decide that itself — a second notion of "already
/// queued" out here is exactly what AC-3 forbids.
pub async fn reviews_enqueue(workspace: &str, seed: Option<&str>) -> Result<()> {
    let client = Client::from_config()?;
    let mut body = serde_json::json!({ "workspace_id": workspace });
    if let Some(seed) = seed {
        body["seed"] = serde_json::Value::String(seed.to_string());
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
