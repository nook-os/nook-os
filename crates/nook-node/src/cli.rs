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
}

/// `nook login --token nook_user_…` — act as yourself, not as this machine.
///
/// Verifies the token before writing it, because a credential that silently
/// doesn't work is worse than one that obviously doesn't.
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
    println!(
        "✓ {sname} — {runtime} on {}",
        location
            .get("node_name")
            .and_then(Value::as_str)
            .unwrap_or("?")
    );
    println!("  nook send {sname} 'your prompt'");
    println!("  nook read {sname}");
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
    println!("── {session} · runtime={} · status={} ──", last.0, last.1);
    println!("{}", last.2);
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
        "nodes" => vec!["name", "platform", "status", "last_seen_at"],
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

fn cell(row: &Value, key: &str) -> String {
    match row.get(key) {
        None | Some(Value::Null) => "-".into(),
        Some(Value::String(s)) if s.is_empty() => "-".into(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
    }
}

fn print_table(resource: &str, rows: &[Value]) {
    let cols = columns(resource, &rows[0]);
    let headers: Vec<String> = cols.iter().map(|c| c.to_uppercase()).collect();
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
    let line = |cells: &[String]| {
        let mut out = String::new();
        for (i, v) in cells.iter().enumerate() {
            let pad = widths[i] - v.chars().count();
            out.push_str(v);
            if i + 1 < cells.len() {
                out.push_str(&" ".repeat(pad + 2));
            }
        }
        println!("{}", out.trim_end());
    };
    line(&headers);
    for row in &body {
        line(row);
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
