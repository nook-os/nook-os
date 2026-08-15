//! `nook ports list` — the listeners this workspace declared, joined to the
//! numbers this process actually holds (MAIN-597).
//!
//! Two halves of one question live in two places and neither can answer alone.
//! The DECLARATION is the workspace's — which listeners exist, which of them
//! serve a UI, under what path — and it is resolved by the control plane
//! (MAIN-596's `port_leases::browsable_targets`, so the rule is not re-derived
//! per caller). The NUMBERS are in this process's environment, put there by
//! whoever started the session or the build run. A recorder wanting a URL to
//! open needs both, and before this it had neither: `env | grep PORT` says what
//! was leased but not what any of it serves.
//!
//! **A number is only ever read from the variable the declaration names.** No
//! literal, no `NOOK_WEB_PORT` fallback — that hardcoding is precisely what
//! MAIN-596 exists to end, and a repo whose UI is `ADMIN_PORT` would be opened
//! at somebody else's stack.

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::cli::{pick_one, Client};
use crate::style;

/// One declared listener, as this environment sees it.
#[derive(Debug, PartialEq, Eq)]
pub struct Listener {
    pub name: String,
    pub env: String,
    pub browsable: bool,
    /// Where the UI is served, `/` unless the declaration says otherwise. Junk
    /// on a listener nobody can open, which is why it is only ever printed
    /// beside `browsable`.
    pub path: String,
    /// What the named variable holds here, or `None` when it is unset — an
    /// optional listener that went unleased, or a shell outside nook.
    pub port: Option<u16>,
}

impl Listener {
    /// The address to open, for a browsable listener whose port is in hand.
    ///
    /// `127.0.0.1` rather than `localhost`: on a machine resolving that to
    /// `::1` first, a dev server bound to v4 only is a connection refused that
    /// reads as "the app is broken".
    pub fn url(&self) -> Option<String> {
        let port = self.port?;
        if !self.browsable {
            return None;
        }
        Some(format!(
            "http://127.0.0.1:{port}{}",
            normalize_path(&self.path)
        ))
    }
}

/// A declared path made safe to concatenate: `admin` and `/admin` are the same
/// intention, and an empty one is the root.
fn normalize_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return "/".to_string();
    }
    if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

/// `nook ports list [--browsable]` — what this workspace declared, and what of
/// it this process holds.
pub async fn list(workspace: Option<&str>, browsable_only: bool, json_out: bool) -> Result<()> {
    let client = Client::from_config()?;
    let id = resolve_workspace(&client, workspace).await?;
    let path = if browsable_only {
        format!("/api/v1/workspaces/{id}/browsable")
    } else {
        format!("/api/v1/workspaces/{id}/ports")
    };
    let rows = client.get(&path).await?;
    let listeners = read_listeners(
        rows.as_array().map(Vec::as_slice).unwrap_or_default(),
        browsable_only,
        |var| std::env::var(var).ok(),
    );

    if json_out {
        println!("{}", serde_json::to_string_pretty(&as_json(&listeners))?);
        return Ok(());
    }
    for line in render(&listeners, browsable_only) {
        println!("{line}");
    }
    Ok(())
}

/// The workspace this command is about: the one named, else the one this
/// session or build run is already in.
///
/// `NOOK_WORKSPACE_ID` is exported into every session (MAIN-367) and every loop
/// job, so the common case — an agent asking what it can open in the repo it is
/// standing in — takes no argument at all.
async fn resolve_workspace(client: &Client, want: Option<&str>) -> Result<String> {
    let Some(want) = want.filter(|w| !w.trim().is_empty()) else {
        return std::env::var("NOOK_WORKSPACE_ID")
            .ok()
            .filter(|w| !w.trim().is_empty())
            .context(
                "no workspace — run this inside a nook session or a loop job, or name one with \
                 --workspace <name|id>",
            );
    };
    let rows = client.get("/api/v1/workspaces").await?;
    // Name, slug or id — the same three `nook start` and `nook set ports`
    // accept. A workspace addressed by slug everywhere else must not be
    // unfindable here.
    let ws = pick_one(
        rows.as_array().cloned().unwrap_or_default(),
        want,
        &["name", "slug", "id"],
        "workspaces",
    )?;
    Ok(ws["id"]
        .as_str()
        .context("a workspace with no id")?
        .to_string())
}

/// Both endpoints' rows, read into one shape.
///
/// `/browsable` answers `BrowsableTarget` — already filtered, no `browsable`
/// field to read — and `/ports` answers the whole declaration. Reading them
/// into one type is what keeps a single render and a single JSON contract, so
/// `--browsable` narrows the list rather than changing what a caller parses.
fn read_listeners(
    rows: &[Value],
    all_browsable: bool,
    env: impl Fn(&str) -> Option<String>,
) -> Vec<Listener> {
    rows.iter()
        .map(|r| {
            let var = r["env"].as_str().unwrap_or_default().to_string();
            Listener {
                name: r["name"].as_str().unwrap_or("—").to_string(),
                port: env(&var).and_then(|v| v.trim().parse().ok()),
                env: var,
                browsable: all_browsable || r["browsable"].as_bool().unwrap_or(false),
                path: r["path"].as_str().unwrap_or("/").to_string(),
            }
        })
        .collect()
}

/// The declaration's own fields, plus what this environment resolved them to.
///
/// A superset of the API shape rather than a different one: `name`, `env` and
/// `path` are the server's, verbatim, and `port`/`url` are the join only a
/// process holding the leases can make. A caller that wanted the API shape
/// alone would be asking the endpoint.
fn as_json(listeners: &[Listener]) -> Value {
    Value::Array(
        listeners
            .iter()
            .map(|l| {
                json!({
                    "name": l.name,
                    "env": l.env,
                    "browsable": l.browsable,
                    "path": l.path,
                    "port": l.port,
                    "url": l.url(),
                })
            })
            .collect(),
    )
}

fn render(listeners: &[Listener], browsable_only: bool) -> Vec<String> {
    if listeners.is_empty() {
        return vec![if browsable_only {
            // AC-3b: a gap in the declaration, not a failure. Said in the words
            // that name the fix, because the reader is usually an agent that
            // has just decided it had something to record.
            style::dim(
                "no browsable target declared — nothing in this workspace says it serves a UI \
                 (set one on the Ports panel, or mark a listener `browsable` in .nook.toml)",
            )
        } else {
            style::dim("this workspace declares no listeners — it binds nothing")
        }];
    }
    let width = listeners.iter().map(|l| l.name.len()).max().unwrap_or(4);
    listeners
        .iter()
        .map(|l| {
            let head = format!("  {:width$}  {}", l.name, l.env, width = width);
            match (l.port, l.url()) {
                (Some(port), Some(url)) => format!("{head}  {port}  {url}"),
                (Some(port), None) => format!("{head}  {port}"),
                (None, _) => format!(
                    "{head}  {}",
                    style::dim(&format!(
                        "unset here — {} is not in this environment",
                        l.env
                    ))
                ),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<Value> {
        vec![
            json!({"name": "web", "env": "NOOK_WEB_PORT", "browsable": true, "path": "/"}),
            json!({"name": "admin", "env": "ADMIN_PORT", "browsable": true, "path": "/admin"}),
            json!({"name": "pg", "env": "PG_PORT", "browsable": false, "path": "/"}),
        ]
    }

    fn leased(var: &str) -> Option<String> {
        match var {
            "NOOK_WEB_PORT" => Some("4180".into()),
            "PG_PORT" => Some("4182".into()),
            _ => None,
        }
    }

    /// The whole point of the command: the URL comes from the variable the
    /// DECLARATION names, so a repo serving its UI on `ADMIN_PORT` is opened
    /// there and never on a hardcoded web port.
    #[test]
    fn the_url_is_built_from_the_declared_variable_and_path() {
        let listeners = read_listeners(&rows(), false, leased);
        assert_eq!(
            listeners[0].url().as_deref(),
            Some("http://127.0.0.1:4180/")
        );
        // Declared, browsable, and simply not leased in this process — no URL
        // to open, rather than one pointing at whatever holds that port.
        assert_eq!(listeners[1].url(), None);
        assert_eq!(listeners[1].port, None);
    }

    #[test]
    fn a_leased_listener_nobody_can_open_has_no_url() {
        let listeners = read_listeners(&rows(), false, leased);
        assert_eq!(listeners[2].port, Some(4182), "it is leased");
        assert_eq!(listeners[2].url(), None, "but it serves no UI");
    }

    /// `/browsable` answers rows with no `browsable` field — they are the
    /// filter's output — so reading them must not silently produce a list
    /// nothing in it is browsable.
    #[test]
    fn the_browsable_endpoints_rows_are_all_browsable() {
        let targets = vec![json!({"name": "web", "env": "NOOK_WEB_PORT", "path": "/app"})];
        let listeners = read_listeners(&targets, true, leased);
        assert!(listeners[0].browsable);
        assert_eq!(
            listeners[0].url().as_deref(),
            Some("http://127.0.0.1:4180/app")
        );
    }

    #[test]
    fn a_declared_path_is_normalized_before_it_is_joined() {
        assert_eq!(normalize_path("admin"), "/admin");
        assert_eq!(normalize_path(""), "/");
        assert_eq!(normalize_path(" /admin "), "/admin");
    }

    /// AC-3b's silence: a workspace declaring nothing browsable says so, and
    /// says it in a way that names the fix.
    #[test]
    fn no_browsable_target_is_a_sentence_not_an_empty_table() {
        let out = render(&[], true);
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("no browsable target declared"), "{out:?}");
    }

    #[test]
    fn json_carries_the_declaration_and_the_resolved_url() {
        let listeners = read_listeners(&rows(), false, leased);
        let v = as_json(&listeners);
        assert_eq!(v[0]["url"], "http://127.0.0.1:4180/");
        assert_eq!(v[0]["port"], 4180);
        assert_eq!(v[1]["url"], Value::Null, "unleased: null, never a guess");
        assert_eq!(v[1]["path"], "/admin", "the declaration, verbatim");
        assert_eq!(v[2]["browsable"], false);
    }
}
