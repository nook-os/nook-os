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
    /// Build a client from the local node config.
    pub fn from_config() -> Result<Self> {
        let cfg = NodeConfig::load().context(
            "no node config — run `nook setup` to connect this machine to a control plane",
        )?;
        Ok(Self {
            base: cfg.server.trim_end_matches('/').to_string(),
            token: cfg.node_token,
            http: reqwest::Client::new(),
        })
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
        // `nook get nodes crimson` filters by name/slug/id.
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
