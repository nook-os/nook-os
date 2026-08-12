//! `.nook.toml` — what a repo says about itself (MAIN-359).
//!
//! MAIN-301 built the whole port-leasing machine and left one gap: the only way
//! to tell nook what a repo binds was `PUT /workspaces/{id}/ports`, a curl
//! command against one workspace. Clone the same repo into another nook and the
//! knowledge was gone.
//!
//! A committed file gives the declaration the three things a database row
//! cannot: history (when the port list changed and why), review (the change
//! arrives in a PR like any other), and portability (clone it anywhere and it
//! configures itself).
//!
//! **A GENERAL settings file that happens to have a ports section.** Unknown
//! top-level keys are ignored rather than rejected, so the next setting is a new
//! section instead of a format change — and, just as importantly, an older
//! control plane reading a newer file keeps working instead of declaring the
//! whole file broken. That tolerance is the reason this is `.nook.toml` and not
//! `.nook-ports.toml`.
//!
//! It is a DECLARATION, never a detection: nothing here reads the repo's source
//! looking for hardcoded ports (NG-5). What the file says is what nook believes.

use nook_types::PortRequirement;

/// Every top-level section this build understands. Deliberately a struct with
/// ONE field and no `deny_unknown_fields`: serde ignores what it does not know,
/// which is AC-2, and adding `[build]` later is adding a field here.
#[derive(Debug, Default, serde::Deserialize)]
struct RepoSettings {
    #[serde(default)]
    ports: Vec<PortRequirement>,
}

/// Why a `.nook.toml` could not be believed.
///
/// Every variant names the file and the specific problem, because the caller's
/// job on any of them is identical — keep the stored requirements and tell
/// somebody — and the only useful difference is what it says (AC-4).
#[derive(Debug)]
pub enum SettingsError {
    Parse(String),
    DuplicateName(String),
    DuplicateEnv(String),
    BadEnv(String),
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(e) => write!(f, ".nook.toml is not valid TOML: {e}"),
            Self::DuplicateName(n) => write!(
                f,
                ".nook.toml declares two ports called `{n}` — names key the \
                 leases, so the second would silently replace the first"
            ),
            Self::DuplicateEnv(e) => write!(
                f,
                ".nook.toml declares two ports writing `{e}` — one would \
                 silently win and the other listener would get no port"
            ),
            Self::BadEnv(e) => write!(
                f,
                ".nook.toml declares `{e}`, which is not a usable environment \
                 variable name (letters, digits and underscore, not starting \
                 with a digit)"
            ),
        }
    }
}

/// Parse a repo's `.nook.toml`.
///
/// `Ok(None)` means the file declared no `[[ports]]` KEY at all — the repo has
/// a settings file but says nothing about ports, which is not the same as
/// saying it binds none. `Ok(Some(vec![]))` is the explicit "this repo binds
/// nothing" (AC-6), and the two have to stay distinguishable because one of
/// them caps the workspace and the other does not.
pub fn parse(source: &str) -> Result<Option<Vec<PortRequirement>>, SettingsError> {
    // Parsed twice on purpose. The typed parse is the answer; the raw one is
    // the only way to tell "no `ports` key" from "an empty `ports` array",
    // because `#[serde(default)]` collapses both to an empty Vec — and AC-6
    // turns on exactly that difference.
    let raw: toml::Value =
        toml::from_str(source).map_err(|e| SettingsError::Parse(e.to_string()))?;
    let declared = raw.get("ports").is_some();

    let settings: RepoSettings =
        toml::from_str(source).map_err(|e| SettingsError::Parse(e.to_string()))?;
    if !declared {
        return Ok(None);
    }
    validate(&settings.ports)?;
    Ok(Some(settings.ports))
}

/// The two collisions that would fail quietly at lease time, plus an env name
/// the node could not export.
///
/// Rejected here rather than at the broker because this is where a human can
/// still see what they typed. A duplicate `name` breaks the per-name lease
/// uniqueness the merged migration relies on
/// (`session_port_leases_one_per_name`); a duplicate `env` means one listener
/// gets no port and nothing says so.
fn validate(ports: &[PortRequirement]) -> Result<(), SettingsError> {
    let mut names: std::collections::BTreeSet<&str> = Default::default();
    let mut envs: std::collections::BTreeSet<&str> = Default::default();
    for p in ports {
        if !p.env.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || p.env.is_empty()
            || p.env.starts_with(|c: char| c.is_ascii_digit())
        {
            return Err(SettingsError::BadEnv(p.env.clone()));
        }
        if !names.insert(p.name.as_str()) {
            return Err(SettingsError::DuplicateName(p.name.clone()));
        }
        if !envs.insert(p.env.as_str()) {
            return Err(SettingsError::DuplicateEnv(p.env.clone()));
        }
    }
    Ok(())
}

/// The file's name at a repo root. One constant, because the node is asked for
/// it by name and the parser's error messages quote it.
pub const FILE: &str = ".nook.toml";

/// Read `.nook.toml` out of one checkout and, if it declares ports, store them
/// on the workspace (AC-3).
///
/// **The repo's answer wins, and re-syncs on every scan** — that is the point of
/// committing it. But only when the file actually says something: a repo with no
/// file, or a file with no `[[ports]]` key, leaves whatever is stored alone.
/// Treating "no file" as "declares nothing" would let adding nook to a repo
/// silently wipe a declaration somebody made through the API.
///
/// Every failure path keeps the stored requirements and says so out loud
/// (AC-4). A `.nook.toml` with a typo in it must never read as "this repo binds
/// nothing" — that state caps the workspace, and a silent cap for a typo is the
/// worst outcome this card can produce.
pub async fn sync_from_checkout(
    state: &crate::state::AppState,
    tenant: nook_types::TenantId,
    workspace: nook_types::WorkspaceId,
    node: nook_types::NodeId,
    checkout_path: &str,
) {
    use base64::Engine;

    // Every arm below used to be a bare `return`. That made a working sync and
    // a broken one produce byte-identical output — nothing — so the only way to
    // tell which you had was to query the database and know the field's name.
    // A feature whose failure is indistinguishable from its success is one
    // nobody can operate, so each dead end now says which one it was.
    let Some(rx) = state.registry.request_op(node, |request_id| {
        nook_proto::ControlToNode::ReadWorkspaceFile {
            request_id,
            checkout_path: checkout_path.to_string(),
            name: FILE.to_string(),
        }
    }) else {
        // Ordinary: the node reported a scan and dropped before we asked.
        tracing::debug!(%workspace, %node, "no {FILE} read — node went away after its scan");
        return;
    };
    let payload = match tokio::time::timeout(std::time::Duration::from_secs(15), rx).await {
        Ok(Ok(p)) => p,
        // NOT ordinary. A node that never answers leaves the declaration stuck
        // at whatever it last was, and every port this repo asks for silently
        // stops being requested — the exact shape of failure this card exists
        // to prevent, so it is a warning rather than a debug line.
        Ok(Err(e)) => {
            tracing::warn!(%workspace, %node, error = %e, "{FILE} read failed — port declaration not refreshed");
            return;
        }
        Err(_) => {
            tracing::warn!(%workspace, %node, checkout_path, "{FILE} read timed out after 15s — port declaration not refreshed");
            return;
        }
    };
    if !payload.ok {
        // The ordinary case for most repos: they simply do not declare.
        tracing::debug!(%workspace, checkout_path, "no {FILE} in this checkout");
        return;
    }
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&payload.message) else {
        tracing::warn!(%workspace, %node, "{FILE} came back undecodable — port declaration not refreshed");
        return;
    };
    let Ok(source) = String::from_utf8(bytes) else {
        report_invalid(state, tenant, workspace, checkout_path, "it is not UTF-8").await;
        return;
    };
    apply(state, tenant, workspace, checkout_path, &source).await;
}

/// Parse one file's SOURCE and store what it declares.
///
/// Split from the read so the decision — what a given file does to the stored
/// requirements — is testable against a real database without a node on the
/// other end of a socket. The read half is a round trip and nothing else; this
/// half is every rule the card is about.
pub async fn apply(
    state: &crate::state::AppState,
    tenant: nook_types::TenantId,
    workspace: nook_types::WorkspaceId,
    checkout_path: &str,
    source: &str,
) {
    match parse(source) {
        Err(e) => report_invalid(state, tenant, workspace, checkout_path, &e.to_string()).await,
        // A settings file that says nothing about ports changes nothing — but
        // say so, because "present and silent about ports" and "absent" have
        // very different fixes and used to look the same from outside.
        Ok(None) => {
            tracing::debug!(%workspace, checkout_path, "{FILE} declares no ports — leaving the stored requirement alone");
        }
        Ok(Some(ports)) => {
            let count = ports.len();
            let required = ports.iter().filter(|p| p.required).count();
            let stored = serde_json::to_value(&ports).unwrap_or(serde_json::Value::Null);
            match state
                .workspaces
                .set_port_requirements(tenant, workspace, Some(stored))
                .await
            {
                // Debug, not info: this re-reads on EVERY scan, so an info line
                // here would be one per workspace per scan forever. The number
                // of REQUIRED listeners rides along because that is the figure
                // that decides whether a session can start at all — a node with
                // no range refuses every one of them.
                Ok(_) => tracing::debug!(
                    %workspace,
                    listeners = count,
                    required,
                    "stored {FILE}'s port declaration"
                ),
                Err(e) => {
                    tracing::warn!(%workspace, error = %e, "could not store .nook.toml's ports")
                }
            }
        }
    }
}

async fn report_invalid(
    state: &crate::state::AppState,
    tenant: nook_types::TenantId,
    workspace: nook_types::WorkspaceId,
    checkout_path: &str,
    problem: &str,
) {
    tracing::warn!(%workspace, path = %checkout_path, %problem, "unreadable .nook.toml — keeping the stored ports");
    crate::events::record(
        state,
        tenant,
        crate::events::EventDraft::new("workspace.settings_invalid")
            .workspace(workspace)
            .payload(serde_json::json!({
                "file": FILE,
                "path": checkout_path,
                "error": problem,
            })),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_three_port_file_parses_with_the_struct_defaults() {
        let ports = parse(
            r#"
[[ports]]
name = "web"
env  = "PORT"

[[ports]]
name = "api"
env  = "API_PORT"
required = true

[[ports]]
name = "debug"
env  = "DEBUG_PORT"
protocol = "udp"
"#,
        )
        .expect("parses")
        .expect("declares ports");

        assert_eq!(ports.len(), 3);
        assert_eq!(ports[0].env, "PORT");
        // The defaults are the STRUCT's, not this parser's — no second shape.
        assert_eq!(ports[0].protocol, "tcp");
        assert!(!ports[0].required, "required defaults false");
        assert!(ports[1].required);
        assert_eq!(ports[2].protocol, "udp");
    }

    #[test]
    fn an_unknown_top_level_section_is_ignored_not_rejected() {
        // AC-2, and the whole reason the file is general rather than
        // ports-only: a newer nook writing `[build]` must not brick an older
        // control plane reading the same repo.
        let ports = parse(
            r#"
[build]
command = "cargo build"

[[ports]]
name = "web"
env  = "PORT"

[sessions]
runtime = "claude"
"#,
        )
        .expect("unknown sections are ignored")
        .expect("ports still parsed");
        assert_eq!(ports.len(), 1);
    }

    #[test]
    fn no_ports_key_is_different_from_an_empty_ports_array() {
        // AC-6. One of these caps the workspace and the other does not, so
        // collapsing them would silently change what a repo is asking for.
        assert!(
            parse("[build]\ncommand = \"x\"\n")
                .expect("parses")
                .is_none(),
            "a file that says nothing about ports has declared nothing"
        );
        assert_eq!(
            parse("ports = []\n").expect("parses"),
            Some(vec![]),
            "an empty array IS a declaration: this repo binds nothing"
        );
    }

    #[test]
    fn malformed_toml_names_the_file_and_the_problem() {
        let err = parse("[[ports]\nname = \"web\"\n").expect_err("not valid TOML");
        let msg = err.to_string();
        assert!(msg.contains(".nook.toml"), "{msg}");
        assert!(matches!(err, SettingsError::Parse(_)));
    }

    #[test]
    fn a_duplicate_name_is_rejected_and_named() {
        let err = parse(
            r#"
[[ports]]
name = "web"
env  = "PORT"
[[ports]]
name = "web"
env  = "OTHER_PORT"
"#,
        )
        .expect_err("two listeners cannot share a name");
        assert!(matches!(err, SettingsError::DuplicateName(ref n) if n == "web"));
        assert!(err.to_string().contains("replace the first"));
    }

    #[test]
    fn a_duplicate_env_is_rejected_and_named_differently() {
        // A distinct error from the duplicate name (AC-5): the consequences
        // differ, and a reader fixing one should not be told about the other.
        let err = parse(
            r#"
[[ports]]
name = "web"
env  = "PORT"
[[ports]]
name = "api"
env  = "PORT"
"#,
        )
        .expect_err("two listeners cannot share a variable");
        assert!(matches!(err, SettingsError::DuplicateEnv(ref e) if e == "PORT"));
        assert!(err.to_string().contains("no port"));
    }

    #[test]
    fn an_env_name_the_node_could_not_export_is_rejected() {
        // The node splices these into a session's environment. A value with a
        // space or an `=` would be dropped or corrupt its neighbours, and
        // neither failure would point back at this file.
        for bad in ["MY PORT", "PORT=8080", "8PORT", ""] {
            let src = format!("[[ports]]\nname = \"web\"\nenv = \"{bad}\"\n");
            assert!(
                matches!(parse(&src), Err(SettingsError::BadEnv(_))),
                "should reject {bad:?}"
            );
        }
        assert!(parse("[[ports]]\nname = \"web\"\nenv = \"PORT_2\"\n").is_ok());
    }

    #[test]
    fn this_repos_own_file_parses() {
        // AC-7's file, checked by the build rather than by a human remembering
        // to look. It is the first real example, so a typo in it is a typo in
        // the documentation everyone will copy.
        let src = include_str!("../../../../.nook.toml");
        let ports = parse(src).expect("our own file parses").expect("declares");
        assert!(!ports.is_empty(), "declares at least one listener");

        // The two collisions the server and the lease table also reject, caught
        // here for our OWN file so a bad edit fails the build rather than a
        // session start.
        let mut names: Vec<&str> = ports.iter().map(|p| p.name.as_str()).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(before, names.len(), "listener names are unique: {ports:?}");
        let mut envs: Vec<&str> = ports.iter().map(|p| p.env.as_str()).collect();
        envs.sort_unstable();
        envs.dedup();
        assert_eq!(
            before,
            envs.len(),
            "listener variables are unique: {ports:?}"
        );
    }

    /// Declaring a port and never reading it is the half of MAIN-376 that is
    /// easy to skip — nook leases a number, exports the variable, and an app
    /// that ignores it collides anyway. The declaration is only true if
    /// something consumes it, so the build checks that rather than trusting it.
    ///
    /// Compose is the consumer for this repo: every host binding is
    /// `${VAR:-<previous default>}`. Both directions are asserted, because each
    /// catches a different mistake — a declared listener nothing reads, and a
    /// published port nobody declared (the collision this card exists to fix).
    #[test]
    fn every_declared_port_is_read_by_the_compose_file() {
        let declared = parse(include_str!("../../../../.nook.toml"))
            .expect("our own file parses")
            .expect("declares");
        let compose = include_str!("../../../../docker-compose.yml");

        for p in &declared {
            assert!(
                compose.contains(&format!("${{{}:-", p.env)),
                "{} is declared but no compose binding reads it",
                p.env
            );
        }

        // Every published host port must come from a variable. A bare
        // `- "5432:5432"` is exactly the hardcoded literal that makes two
        // checkouts fight, so it fails here.
        //
        // Keyed on being inside a `ports:` block, NOT on quoting. The first cut
        // matched `- "`, which got it wrong in both directions: an unquoted
        // `- 1234:1234` is valid YAML and slipped straight through, and a quoted
        // volume or command containing a colon would have failed for no reason.
        // The block is the thing that means "these are published ports".
        let mut in_ports = false;
        let mut ports_indent = 0usize;
        for line in compose.lines() {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            let indent = line.len() - line.trim_start().len();
            // A list item under `ports:` may sit at the SAME indent as the key
            // — both `ports:\n  - "x"` and `ports:\n- "x"` are valid YAML — so
            // only a non-item line at that indent means we have left the block.
            // Keying purely on indent closed it before the first entry in the
            // flush style and read nothing.
            if in_ports && indent <= ports_indent && !line.trim_start().starts_with("- ") {
                in_ports = false;
            }
            if line.trim_start().starts_with("ports:") {
                in_ports = true;
                ports_indent = indent;
                continue;
            }
            if !in_ports {
                continue;
            }
            let Some(entry) = line.trim().strip_prefix("- ") else {
                continue;
            };
            // Strip quotes and any trailing `# comment`, then take the HOST side.
            let entry = entry.trim().trim_matches('"').trim_matches('\'');
            let entry = entry.split('#').next().unwrap_or("").trim();
            let entry = entry.trim_matches('"').trim_matches('\'');
            if !entry.contains(':') {
                continue; // a bare container port publishes a RANDOM host port
            }
            assert!(
                entry.starts_with("${"),
                "docker-compose.yml publishes {entry:?} on a literal host port — declare it in \
                 .nook.toml and read it as ${{VAR:-default}}, or two checkouts of this repo collide"
            );
        }
    }

    /// A service that writes into the checkout must not write into it as root
    /// (MAIN-537 AC-1).
    ///
    /// Here beside the port guard because it is the same kind of rule about the
    /// same file — a property of this repo's compose that no reviewer should
    /// have to remember. What it prevents is specific and has happened: a build
    /// worktree ended up holding 14,918 root-owned files, and the node that
    /// created the tree, running as an ordinary user, could not delete it. Every
    /// merged card then leaked its checkout, permanently.
    ///
    /// Keyed on bind mounts because that is exactly the reach: a named volume
    /// lives in Docker's own storage and `down -v` collects it whatever its
    /// owner, while a `./` source is a directory on the host that outlives the
    /// container and belongs to whoever wrote it.
    #[test]
    fn every_service_that_bind_mounts_the_checkout_runs_as_the_host_user() {
        let compose = include_str!("../../../../docker-compose.yml");
        const USER: &str = r#"user: "${NOOK_UID:-1000}:${NOOK_GID:-1000}""#;

        let mut service = String::new();
        let mut declares_user = false;
        let mut binds_checkout = false;
        let mut offenders: Vec<String> = Vec::new();
        let close = |service: &str, binds: bool, user: bool, offenders: &mut Vec<String>| {
            if binds && !user {
                offenders.push(service.to_string());
            }
        };
        let mut in_services = false;
        for line in compose.lines() {
            let indent = line.len() - line.trim_start().len();
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if indent == 0 {
                in_services = trimmed == "services:";
                close(&service, binds_checkout, declares_user, &mut offenders);
                service.clear();
                continue;
            }
            if !in_services {
                continue;
            }
            // A service key: two spaces in, ends in a colon, nothing after it.
            if indent == 2 && trimmed.ends_with(':') && !trimmed.contains(' ') {
                close(&service, binds_checkout, declares_user, &mut offenders);
                service = trimmed.trim_end_matches(':').to_string();
                declares_user = false;
                binds_checkout = false;
                continue;
            }
            declares_user |= trimmed == USER;
            // `- .:/app`, `- ./frontend:/app/frontend` — a source on the host.
            binds_checkout |= trimmed.starts_with("- .") && trimmed.contains(':');
        }
        close(&service, binds_checkout, declares_user, &mut offenders);

        assert!(
            offenders.is_empty(),
            "these services bind-mount the checkout and would write into it as root: {offenders:?} \
             — add `{USER}`, or the build worktrees they touch can never be pruned"
        );
        // The guard is only worth having if it can SEE the services it guards.
        assert!(
            compose.matches(USER).count() >= 6,
            "the compose file should still declare the host user on every bind-mounting service"
        );
    }
}
