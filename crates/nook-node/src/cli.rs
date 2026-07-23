//! `nook get …` and friends: a kubectl-shaped client for the control plane.
//!
//! Authentication reuses the node token already stored in `node.toml` by
//! `nook setup` — if this machine is part of a NookOS instance, its CLI can
//! talk to that instance with no extra login.

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::config::NodeConfig;

pub struct Client {
    base: String,
    token: String,
    http: reqwest::Client,
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
pub async fn whoami() -> Result<()> {
    let client = Client::from_config()?;
    let me = client.get("/api/v1/auth/me").await?;
    let field = |a: &str, b: &str| {
        me.get(a)
            .and_then(|o| o.get(b))
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string()
    };
    println!("server:  {}", client.base);
    println!(
        "as:      {} ({})",
        field("user", "email"),
        if client.is_user() {
            "user token — can drive any node"
        } else {
            "node token — confined to this machine"
        }
    );
    println!("tenant:  {}", field("tenant", "slug"));
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

/// Find one session by name or id. Names are what people (and agents) can
/// remember; ids are what survives a rename.
async fn find_session(client: &Client, want: &str) -> Result<Value> {
    let list = client.get("/api/v1/sessions").await?;
    let rows = list.as_array().cloned().unwrap_or_default();
    rows.into_iter()
        .find(|r| {
            ["name", "id"]
                .iter()
                .filter_map(|k| r.get(*k).and_then(Value::as_str))
                .any(|v| v.eq_ignore_ascii_case(want))
        })
        .with_context(|| format!("no session named '{want}' — try `nook get sessions`"))
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
        other => bail!(
            "unknown resource '{other}' — try: nodes, sessions, workspaces, \
             secrets, tasks, events, themes"
        ),
    })
}

/// `nook get <resource>` — a table by default, raw JSON with --json.
pub async fn get(kind: &str, name: Option<&str>, json: bool) -> Result<()> {
    let resource = resolve_resource(kind)?;
    let client = Client::from_config()?;

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
    let rows: Vec<Value> = match (resource, name) {
        // `nook get nodes buildbox` filters by name/slug/id.
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
    };

    if rows.is_empty() {
        eprintln!("No {resource} found.");
        return Ok(());
    }
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
            "capabilities.agent_version",
            "capabilities.cpus",
            "capabilities.memory",
            "capabilities.runtimes",
            "last_seen_at",
        ],
        "sessions" => vec!["name", "runtime", "status", "created_at"],
        "workspaces" => vec!["name", "slug", "git_remote_normalized"],
        "secrets" => vec!["workspace", "name", "updated_at"],
        "tasks" => vec!["title", "column_id", "branch", "pr_url"],
        "events" => vec!["occurred_at", "kind", "actor_type"],
        "themes" => vec!["name", "slug"],
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
    let mut node = row;
    for part in key.split('.') {
        match node.get(part) {
            Some(v) => node = v,
            None => return "-".into(),
        }
    }
    render_value(key, node)
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

fn print_table(resource: &str, rows: &[Value]) {
    let cols = columns(resource, &rows[0]);
    // Header names the field, not its path: `CAPABILITIES.AGENT_VERSION` is a
    // location, `AGENT_VERSION` is a column.
    let headers: Vec<String> = cols
        .iter()
        .map(|c| c.rsplit('.').next().unwrap_or(c).to_uppercase())
        .collect();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|r| cols.iter().map(|c| cell(r, c)).collect())
        .collect();
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
pub async fn delete(kind: &str, name: &str) -> Result<()> {
    let resource = resolve_resource(kind)?;
    if !matches!(resource, "sessions" | "workspaces" | "tasks") {
        bail!("delete is only supported for sessions, workspaces and tasks");
    }
    let client = Client::from_config()?;
    let list = client.get(&format!("/api/v1/{resource}")).await?;
    let found = list
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .find(|r| {
            ["name", "slug", "id", "title"]
                .iter()
                .filter_map(|k| r.get(*k).and_then(Value::as_str))
                .any(|v| v.eq_ignore_ascii_case(name))
        });
    let Some(row) = found else {
        bail!("no {resource} named '{name}'");
    };
    let id = row
        .get("id")
        .and_then(Value::as_str)
        .context("row has no id")?;
    client.delete(&format!("/api/v1/{resource}/{id}")).await?;
    println!("✓ Deleted {} '{name}'", resource.trim_end_matches('s'));
    Ok(())
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

pub async fn current_session_workspace(client: &Client) -> Option<SessionWorkspace> {
    let sid = std::env::var("NOOK_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty())?;
    let session = client.get(&format!("/api/v1/sessions/{sid}")).await.ok()?;
    let id = session.get("workspace_id")?.as_str()?.to_string();
    // The name is for humans only; if the lookup fails, the id still confines.
    let name = client
        .get(&format!("/api/v1/workspaces/{id}"))
        .await
        .ok()
        .and_then(|w| w.get("name").and_then(|v| v.as_str()).map(str::to_string))
        .unwrap_or_else(|| "workspace".into());
    Some(SessionWorkspace { id, name })
}

/// `nook workspace current` — which workspace is this session in?
///
/// The seam `/loop-spec` uses to stamp a new ticket with the workspace it was
/// filed from. Prints nothing (and exits 0) outside a workspace session, so a
/// caller can treat empty output as "unscoped" without special-casing an error.
pub async fn workspace_current(json: bool) -> Result<()> {
    let client = Client::from_config()?;
    match current_session_workspace(&client).await {
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

// One parameter per CLI flag by design — this is the dispatch seam for
// `nook tasks`, and a struct would just move the same list one hop away.
#[allow(clippy::too_many_arguments)]
pub async fn tasks(
    board: Option<&str>,
    labels: &[String],
    not_labels: &[String],
    assignee: Option<&str>,
    column_type: Option<&str>,
    unblocked: bool,
    workspace: Option<&str>,
    all_workspaces: bool,
    json: bool,
) -> Result<()> {
    let client = Client::from_config()?;
    let mut q: Vec<String> = Vec::new();
    if let Some(b) = board {
        q.push(format!("board={b}"));
    }

    // Confinement. An explicit `--workspace` wins; otherwise, unless the caller
    // asked for `--all-workspaces`, a command running inside a workspace session
    // scopes to that workspace by default — so a builder agent cannot see, and
    // therefore cannot take, another repo's work just by forgetting a flag.
    if let Some(w) = workspace {
        q.push(format!(
            "workspace={}",
            resolve_workspace(&client, w).await?
        ));
    } else if !all_workspaces {
        if let Some(ws) = current_session_workspace(&client).await {
            q.push(format!("workspace={}", ws.id));
        }
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
    if unblocked {
        q.push("is_blocked=false".into());
    }
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
        if let Some(here) = current_session_workspace(&client).await {
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

    let body = match column_type {
        Some(c) => serde_json::json!({ "column_type": c }),
        None => serde_json::json!({}),
    };
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
    let rows = rows.as_array().cloned().unwrap_or_default();
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
