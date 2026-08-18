//! `nook secrets set|list|rm|import` — named secret items (MAIN-625).
//!
//! A new noun group, per docs/cli-style.md; the top level stays frozen.
//!
//! **Nothing here can print a value**, and not by discipline: the list endpoint
//! answers `SecretItem`, which has no value field, so there is nothing to
//! render even by accident (AC-4). Writing one is the only direction a value
//! travels through this command.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::cli::{pick_one, Client};

/// Where an item lives, and what it is attached to.
///
/// The scope-dependent shape is what makes the group read as a sentence: a
/// tenant item has nothing to name, a workspace item defaults to the repo the
/// caller is standing in, and a node item must always say which machine.
struct Target {
    scope: &'static str,
    /// `None` only for a tenant item, which can only mean the caller's own.
    id: Option<String>,
}

/// Read `<scope> [<target>] <NAME> [<VALUE>]` off one positional list.
///
/// Positional-with-optional-target rather than a `--workspace` flag, because
/// the two commands people actually type are `nook secrets set workspace
/// API_KEY hunter2` and `nook secrets set node azul SOME_NODE_THING x` — the
/// target is part of the sentence exactly when it is not obvious. A workspace
/// item with no target means the one this session or loop job is already in.
async fn resolve(client: &Client, scope: &str, target: Option<&str>) -> Result<Target> {
    match scope {
        "tenant" => {
            if target.is_some() {
                bail!("a tenant secret takes no target — it is always your own tenant");
            }
            Ok(Target {
                scope: "tenant",
                id: None,
            })
        }
        "workspace" | "repo" => {
            let id = match target {
                Some(want) => {
                    let ws = pick_one(
                        crate::cli::workspaces_all(client).await?,
                        want,
                        &["name", "slug", "id"],
                        "workspaces",
                    )?;
                    ws["id"]
                        .as_str()
                        .context("a workspace with no id")?
                        .to_string()
                }
                // Exported into every session (MAIN-367) and every loop job, so
                // the common case takes no argument at all.
                None => std::env::var("NOOK_WORKSPACE_ID")
                    .ok()
                    .filter(|w| !w.trim().is_empty())
                    .context(
                        "no workspace — run this inside a nook session, or name one: \
                         nook secrets set workspace <name> NAME value",
                    )?,
            };
            Ok(Target {
                scope: "workspace",
                id: Some(id),
            })
        }
        "node" | "machine" => {
            let want = target.context(
                "a node secret needs a machine: nook secrets set node <name> NAME value",
            )?;
            let node = pick_one(
                client
                    .get("/api/v1/nodes")
                    .await?
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
                want,
                &["name", "id"],
                "nodes",
            )?;
            Ok(Target {
                scope: "node",
                id: Some(
                    node["id"]
                        .as_str()
                        .context("a node with no id")?
                        .to_string(),
                ),
            })
        }
        other => bail!("unknown scope '{other}' — try: tenant, workspace, node"),
    }
}

/// Split `[<target>] <NAME> [<VALUE>]` by arity.
///
/// One argument more than the verb needs means a target was given. A `node`
/// item that omits it is not caught here but in [`resolve`], which is the one
/// place that knows a machine cannot be defaulted — saying it twice is how the
/// two come to disagree.
fn split_args(
    args: &[String],
    wants_value: bool,
) -> Result<(Option<String>, String, Option<String>)> {
    let needed = usize::from(wants_value) + 1;
    let (target, rest) = match args.len() {
        n if n == needed + 1 => (Some(args[0].clone()), &args[1..]),
        n if n == needed => (None, args),
        _ => bail!(
            "expected {}",
            if wants_value {
                "<scope> [<target>] <NAME> <VALUE>"
            } else {
                "<scope> [<target>] <NAME>"
            }
        ),
    };
    let name = rest[0].clone();
    let value = wants_value.then(|| rest[1].clone());
    Ok((target, name, value))
}

/// `nook secrets set <scope> [<target>] <NAME> <VALUE>`.
///
/// `-` as the value reads stdin, the convention `nook set-description`
/// established (MAIN-470 AC-1) — which is how a key with newlines in it, or one
/// that must not appear in shell history, gets in.
pub async fn set(scope: &str, args: &[String], json_out: bool) -> Result<()> {
    let (target, name, value) = split_args(args, true)?;
    let value = read_value(&value.expect("wants_value"))?;
    let client = Client::from_config()?;
    let target = resolve(&client, scope, target.as_deref()).await?;
    let mut body = json!({ "scope": target.scope, "name": name, "value": value });
    if let Some(id) = &target.id {
        body["scope_id"] = json!(id);
    }
    let item = client.put("/api/v1/secrets", body).await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&item)?);
        return Ok(());
    }
    println!(
        "{}",
        crate::style::success(&format!("set {name} ({} scope)", target.scope))
    );
    Ok(())
}

/// A lone `-` is stdin; anything else is the value verbatim, dash included.
fn read_value(raw: &str) -> Result<String> {
    if raw != "-" {
        return Ok(raw.to_string());
    }
    use std::io::Read as _;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    // One trailing newline is the shell's, not the secret's: `echo x | nook
    // secrets set …` must not store "x\n" and break every consumer.
    Ok(buf.strip_suffix('\n').unwrap_or(&buf).to_string())
}

/// `nook secrets rm <scope> [<target>] <NAME>`.
pub async fn rm(scope: &str, args: &[String]) -> Result<()> {
    let (target, name, _) = split_args(args, false)?;
    let client = Client::from_config()?;
    let target = resolve(&client, scope, target.as_deref()).await?;
    // The tenant's own id is what a tenant item is keyed on, and only the
    // server knows it — so the delete path takes `tenant` for the id too and
    // the server substitutes. Every other scope names its row.
    let id = match &target.id {
        Some(id) => id.clone(),
        None => tenant_id(&client).await?,
    };
    client
        .delete(&format!("/api/v1/secrets/{}/{id}/{name}", target.scope))
        .await?;
    println!("{}", crate::style::success(&format!("removed {name}")));
    Ok(())
}

/// `nook secrets list [<scope>] [--json]` — names, scopes and timestamps.
pub async fn list(scope: Option<&str>, json_out: bool) -> Result<()> {
    let client = Client::from_config()?;
    let path = match scope {
        Some(s) => format!("/api/v1/secrets?scope={s}"),
        None => "/api/v1/secrets".to_string(),
    };
    let items = client.get(&path).await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }
    let rows = items.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        eprintln!("No secrets found.");
        return Ok(());
    }
    let labels = labels_for(&client, &rows).await;
    crate::cli::print_table("secret_items", &display_rows(&rows, &labels));
    Ok(())
}

/// `nook secrets import <scope> [<target>] [--file <path>]` — a `.env` body as
/// one item per assignment (AC-8).
///
/// Stdin by default, so `cat .env | nook secrets import workspace` is the whole
/// operation. The parse is the SERVER's, not this end's: one reading of a
/// `.env` file, shared by every surface that will ever offer an upload.
pub async fn import(scope: &str, target: Option<&str>, file: Option<&str>) -> Result<()> {
    let content = match file.filter(|f| *f != "-") {
        Some(path) => std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?,
        None => {
            use std::io::Read as _;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        }
    };
    let client = Client::from_config()?;
    let target = resolve(&client, scope, target).await?;
    let mut body = json!({ "scope": target.scope, "content": content });
    if let Some(id) = &target.id {
        body["scope_id"] = json!(id);
    }
    let result = client.post("/api/v1/secrets/import", body).await?;
    let imported = result["imported"].as_array().cloned().unwrap_or_default();
    println!(
        "{}",
        crate::style::success(&format!("imported {} secret(s)", imported.len()))
    );
    for name in &imported {
        println!("  {}", name.as_str().unwrap_or_default());
    }
    // Reported, never silently dropped — the whole point of AC-8's second half.
    for problem in result["problems"].as_array().cloned().unwrap_or_default() {
        let where_ = match problem["line"].as_u64() {
            Some(n) => format!("line {n}"),
            None => "not stored".to_string(),
        };
        eprintln!(
            "{}",
            crate::style::err(&format!(
                "  {where_}: {}",
                problem["reason"].as_str().unwrap_or("unreadable")
            ))
        );
    }
    Ok(())
}

/// The table's rows: the listing's own fields, with each `scope_id` resolved to
/// the name a human recognises.
///
/// Pure, and that is AC-4's other half: the endpoint has nowhere to put a value
/// and neither has this, so what `nook get secrets` and `nook secrets list`
/// print is provably names, scopes and timestamps.
pub(crate) fn display_rows(
    items: &[Value],
    labels: &std::collections::HashMap<String, String>,
) -> Vec<Value> {
    items
        .iter()
        .map(|item| {
            let scope = item["scope"].as_str().unwrap_or("-");
            let id = item["scope_id"].as_str().unwrap_or_default();
            json!({
                "scope": scope,
                // A tenant item is attached to the tenant every row on this
                // table already belongs to, so naming it would be noise.
                "target": if scope == "tenant" {
                    "-".to_string()
                } else {
                    labels.get(id).cloned().unwrap_or_else(|| id.to_string())
                },
                "name": item["name"],
                "updated_at": item["updated_at"],
            })
        })
        .collect()
}

async fn tenant_id(client: &Client) -> Result<String> {
    let tenants = client.get("/api/v1/me/tenants").await?;
    tenants
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|t| t["current"].as_bool() == Some(true))
                .or_else(|| rows.first())
        })
        .and_then(|t| t["id"].as_str())
        .map(str::to_string)
        .context("this token belongs to no tenant")
}

/// Human names for the ids the listing carries, so a table says `nook-os` and
/// `azul` rather than two uuids. Best effort: an id that resolves to nothing
/// is printed as itself.
pub(crate) async fn labels_for(
    client: &Client,
    rows: &[Value],
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let scopes: Vec<&str> = rows.iter().filter_map(|r| r["scope"].as_str()).collect();
    if scopes.contains(&"workspace") {
        for ws in crate::cli::workspaces_all(client).await.unwrap_or_default() {
            if let (Some(id), Some(name)) = (ws["id"].as_str(), ws["name"].as_str()) {
                out.insert(id.to_string(), name.to_string());
            }
        }
    }
    if scopes.contains(&"node") {
        let nodes = client.get("/api/v1/nodes").await.unwrap_or(Value::Null);
        for n in nodes.as_array().cloned().unwrap_or_default() {
            if let (Some(id), Some(name)) = (n["id"].as_str(), n["name"].as_str()) {
                out.insert(id.to_string(), name.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_item_names_its_machine_and_a_workspace_one_need_not() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        let (target, name, value) = split_args(&args(&["API_KEY", "hunter2"]), true).unwrap();
        assert_eq!(
            (target, name, value),
            (None, "API_KEY".into(), Some("hunter2".into()))
        );

        let (target, name, value) = split_args(&args(&["azul", "NODE_THING", "x"]), true).unwrap();
        assert_eq!(
            (target, name, value),
            (Some("azul".into()), "NODE_THING".into(), Some("x".into()))
        );

        let (target, name, _) = split_args(&args(&["nook-os", "API_KEY"]), false).unwrap();
        assert_eq!((target, name), (Some("nook-os".into()), "API_KEY".into()));

        assert!(split_args(&args(&["API_KEY"]), true).is_err());
        assert!(split_args(&args(&["a", "b", "c", "d"]), true).is_err());
    }

    /// AC-4: what a human sees is names, scopes and times. The listing the
    /// server sends has no value in it, and this cannot invent one.
    #[test]
    fn a_rendered_listing_carries_no_value() {
        let items = vec![
            json!({"scope": "tenant", "scope_id": "t-1", "name": "FLEET_KEY",
                   "updated_at": "2026-08-17T00:00:00Z"}),
            json!({"scope": "workspace", "scope_id": "w-1", "name": "REPO_KEY",
                   "updated_at": "2026-08-17T00:00:00Z"}),
        ];
        let labels = std::collections::HashMap::from([("w-1".to_string(), "nook-os".to_string())]);
        let rendered = serde_json::to_string(&display_rows(&items, &labels)).unwrap();
        assert!(rendered.contains("FLEET_KEY") && rendered.contains("nook-os"));
        assert!(
            rendered.contains("2026-08-17"),
            "updated_at must be shown: {rendered}"
        );
        assert!(
            !rendered.contains("value"),
            "no value field may appear: {rendered}"
        );
        // A tenant item names no target; a workspace one names its repo.
        assert_eq!(display_rows(&items, &labels)[0]["target"], "-");
    }

    #[test]
    fn a_lone_dash_reads_stdin_and_anything_else_is_the_value() {
        assert_eq!(read_value("hunter2").unwrap(), "hunter2");
        // A dash among other characters is a value, not the convention.
        assert_eq!(read_value("-abc").unwrap(), "-abc");
    }
}
