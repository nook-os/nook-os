//! Named control planes, and switching between them.
//!
//! Anybody running NookOS seriously has at least two: the one they develop
//! against and the one that matters. Before this, switching meant re-running
//! `nook login` against the other server and typing the token again — so in
//! practice people kept one and edited `auth.toml` by hand, which is how you
//! end up pointing production tooling at localhost without noticing.
//!
//! Modelled on kubectl contexts because the problem is identical and the shape
//! is already in everybody's fingers: a named set of credentials, one of them
//! current, and a command that says which.
//!
//! # Why `auth.toml` is still the active credential
//!
//! Switching WRITES `auth.toml` rather than making every call site consult a
//! context file. That keeps one answer to "what am I talking to" — the same
//! answer `nook login` has always produced — so nothing else in the CLI has to
//! learn about contexts, and a machine that never uses them behaves exactly as
//! it did.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::AuthConfig;

/// One saved control plane.
///
/// Named `ControlPlane` rather than `Context` because `anyhow::Context` is in
/// scope in every module here, and a type that shadows it makes every `?`
/// error message in this file confusing to read.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlPlane {
    pub server: String,
    pub token: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Contexts {
    /// Which one `auth.toml` currently holds. Advisory: the file is the truth,
    /// and this is how `list` puts a marker in the right row without guessing.
    #[serde(default)]
    pub current: Option<String>,
    #[serde(default)]
    pub contexts: BTreeMap<String, ControlPlane>,
}

pub fn contexts_path() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("NOOK_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join("contexts.toml"));
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/nook/contexts.toml"))
}

impl Contexts {
    /// Missing is empty, not an error: the first `save` creates the file.
    pub fn load() -> Result<Self> {
        let path = contexts_path()?;
        match std::fs::read_to_string(&path) {
            Ok(raw) => Ok(toml::from_str(&raw)?),
            Err(_) => Ok(Self::default()),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = contexts_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        // Same rule as auth.toml: this file is a list of passwords.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    /// Which saved context matches what `auth.toml` holds right now?
    ///
    /// Derived rather than trusted, so a hand-edited `auth.toml` or a plain
    /// `nook login` cannot leave the marker pointing at the wrong row — it just
    /// stops matching, and `list` says so.
    pub fn active(&self, auth: Option<&AuthConfig>) -> Option<&str> {
        let auth = auth?;
        let server = auth.server.as_deref()?;
        self.contexts
            .iter()
            .find(|(_, c)| c.server == server && c.token == auth.token)
            .map(|(name, _)| name.as_str())
    }
}

fn normalise(server: &str) -> String {
    server.trim_end_matches('/').to_string()
}

/// Save what `auth.toml` currently holds under a name.
pub fn save(name: &str, server: Option<String>) -> Result<()> {
    let auth = AuthConfig::load().context(
        "not logged in — run `nook login --server <url>` first, then save it under a name",
    )?;
    let server = normalise(
        &server
            .or_else(|| auth.server.clone())
            .context("this login has no server URL — pass --server")?,
    );

    let mut all = Contexts::load()?;
    all.contexts.insert(
        name.to_string(),
        ControlPlane {
            server: server.clone(),
            token: auth.token,
        },
    );
    all.current = Some(name.to_string());
    all.save()?;
    println!(
        "{}",
        crate::style::success(&format!("saved context {name} → {server}"))
    );
    Ok(())
}

/// Point `auth.toml` at a saved context.
pub fn use_context(name: &str) -> Result<()> {
    let mut all = Contexts::load()?;
    let Some(ctx) = all.contexts.get(name).cloned() else {
        let known: Vec<&str> = all.contexts.keys().map(String::as_str).collect();
        if known.is_empty() {
            bail!("no saved contexts — run `nook context save <name>` while logged in");
        }
        bail!("no context named `{name}`. Known: {}", known.join(", "));
    };

    AuthConfig {
        server: Some(ctx.server.clone()),
        token: ctx.token,
    }
    .save()?;
    all.current = Some(name.to_string());
    all.save()?;
    println!(
        "{}",
        crate::style::success(&format!("now using {name} → {}", ctx.server))
    );
    Ok(())
}

pub fn remove(name: &str) -> Result<()> {
    let mut all = Contexts::load()?;
    if all.contexts.remove(name).is_none() {
        bail!("no context named `{name}`");
    }
    if all.current.as_deref() == Some(name) {
        all.current = None;
    }
    all.save()?;
    // Deliberately does NOT touch auth.toml: forgetting where a server is
    // written down should not log you out of the one you are using.
    println!(
        "{}",
        crate::style::success(&format!("removed context {name}"))
    );
    Ok(())
}

/// Print every saved control plane, marking the one in use.
pub fn list() -> Result<()> {
    let all = Contexts::load()?;
    let auth = AuthConfig::load().ok();
    let active = all.active(auth.as_ref());

    if all.contexts.is_empty() {
        println!("No saved contexts.");
        if let Some(a) = &auth {
            println!(
                "\nYou are logged in to {}.\nSave it:  nook context save <name>",
                a.server.as_deref().unwrap_or("(no server)")
            );
        }
        return Ok(());
    }

    let width = all
        .contexts
        .keys()
        .map(String::len)
        .max()
        .unwrap_or(4)
        .max(4);
    println!("   {:width$}  SERVER", "NAME", width = width);
    for (name, ctx) in &all.contexts {
        let here = active == Some(name.as_str());
        let marker = if here { "*" } else { " " };
        let row = format!("{marker}  {name:width$}  {}", ctx.server, width = width);
        println!("{}", if here { crate::style::bold(&row) } else { row });
    }

    // A login that matches nothing is worth saying out loud: it is the state
    // where the marker is absent and everything still "works", which is how
    // somebody ends up running against the wrong control plane.
    if active.is_none() {
        if let Some(a) = &auth {
            println!(
                "\n! Currently logged in to {} — not one of the above.\n  Save it:  nook context save <name>",
                a.server.as_deref().unwrap_or("(no server)")
            );
        }
    }
    Ok(())
}

/// Just the current server, for scripts and prompts.
pub fn current() -> Result<()> {
    let auth = AuthConfig::load().context("not logged in")?;
    let all = Contexts::load()?;
    match all.active(Some(&auth)) {
        Some(name) => println!("{name}\t{}", auth.server.as_deref().unwrap_or("-")),
        None => println!("(unsaved)\t{}", auth.server.as_deref().unwrap_or("-")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(server: &str, token: &str) -> ControlPlane {
        ControlPlane {
            server: server.into(),
            token: token.into(),
        }
    }

    /// The marker is derived from what `auth.toml` holds, not from the
    /// `current` field — so a hand-edited login cannot leave it lying.
    #[test]
    fn the_active_context_is_derived_from_the_credential() {
        let mut all = Contexts {
            current: Some("prod".into()),
            contexts: BTreeMap::new(),
        };
        all.contexts
            .insert("dev".into(), ctx("http://localhost:8080", "t-dev"));
        all.contexts
            .insert("prod".into(), ctx("https://nook.example", "t-prod"));

        let auth = AuthConfig {
            server: Some("http://localhost:8080".into()),
            token: "t-dev".into(),
        };
        assert_eq!(
            all.active(Some(&auth)),
            Some("dev"),
            "`current` said prod, but the credential is dev's — the credential wins"
        );
    }

    /// A login matching no saved context reports none, rather than the nearest
    /// thing. "Close" is how you act on the wrong deployment.
    #[test]
    fn an_unsaved_login_matches_nothing() {
        let mut all = Contexts::default();
        all.contexts
            .insert("prod".into(), ctx("https://nook.example", "t-prod"));

        // Right server, wrong token — a different user on the same deployment.
        let auth = AuthConfig {
            server: Some("https://nook.example".into()),
            token: "someone-else".into(),
        };
        assert_eq!(all.active(Some(&auth)), None);
        assert_eq!(all.active(None), None);
    }

    /// Trailing slashes are a typo, not a different server.
    #[test]
    fn server_urls_normalise() {
        assert_eq!(normalise("https://nook.example/"), "https://nook.example");
        assert_eq!(normalise("https://nook.example"), "https://nook.example");
    }
}
