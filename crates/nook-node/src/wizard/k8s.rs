//! `nook k8s init` — write a Helm values file for a NookOS control plane and
//! print the exact secret + install commands.
//!
//! Hand-off only: it never runs helm or kubectl (MAIN-55 NG-1), and never writes
//! secret material — the `SESSION_SECRET` it generates is *printed* for the
//! operator to put in a Kubernetes Secret, never stored on disk (NG-2). The file
//! is a curated subset of `charts/nook-control/values.yaml` (NG-4).
//!
//! It lives at `~/.nook/k8s/<release>/values.yaml`, so a bare re-run finds and
//! updates the same release instead of leaving the operator to track a stray file
//! in whatever directory they happened to be in. A first line of machine-readable
//! metadata records the install-time fields that are not part of the values
//! (namespace, chart version), so those survive a round-trip too.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::tty::Tty;

/// The OCI chart the hand-off installs from.
const CHART: &str = "oci://ghcr.io/nook-os/charts/nook-control";
const DEFAULT_RELEASE: &str = "nook";
const DEFAULT_NAMESPACE: &str = "nook";
const DEFAULT_SECRET: &str = "nook-control-secrets";

/// One field per CLI flag (AC-2). Anything left `None` is derived, prompted for
/// (with a TTY), or falls back to a stored value from a previous run.
pub struct InitOptions {
    pub release: Option<String>,
    pub namespace: Option<String>,
    pub host: Option<String>,
    pub public_base_url: Option<String>,
    pub web_origin: Option<String>,
    pub ingress_class: Option<String>,
    pub secret_name: Option<String>,
    pub agent: bool,
    pub agent_url: Option<String>,
    pub agent_tls_secret: Option<String>,
    /// `None` → default to `default_chart_version`. `Some("")` → omit `--version`
    /// so helm pulls the latest published chart (AC-3).
    pub chart_version: Option<String>,
    /// The binary's own build version — the default pin (AC-3). Injected by main
    /// so tests do not depend on the crate version.
    pub default_chart_version: String,
    /// The `~/.nook` base. Injected so tests point it at a tempdir instead of
    /// mutating `$HOME` (which would race across parallel tests).
    pub nook_dir: PathBuf,
}

/// The curated values the file carries.
struct Values {
    existing_secret: String,
    public_base_url: String,
    web_origin: String,
    ingress_host: String,
    /// `None` ⇒ the `className` key is omitted (cluster default, AC-3).
    ingress_class: Option<String>,
    agent_enabled: bool,
    agent_public_url: Option<String>,
    agent_tls_secret: Option<String>,
}

/// The install-time fields that are not Helm values but must survive a re-run.
struct Meta {
    release: String,
    namespace: String,
    /// Empty ⇒ no `--version` ⇒ latest.
    chart_version: String,
}

/// What a run produced. Returned so the CLI can print it and tests can assert on
/// it without capturing stdout or opening a terminal. The written values live on
/// disk at `path`; the fresh secret lives only inside `secret_command` (NG-2).
#[derive(Debug)]
pub struct Outcome {
    pub path: PathBuf,
    pub secret_command: String,
    pub helm_command: String,
    pub backed_up: bool,
}

/// CLI entry point: open a terminal if there is one, do the work, print the
/// hand-off. Always exits success once the file is written (NG-1).
pub fn init(opts: InitOptions) -> Result<()> {
    let out = run(opts, Tty::open())?;

    // The hand-off goes to stdout, not /dev/tty: it is the thing an operator
    // copies, so it must survive being piped or captured.
    println!();
    println!("✓ wrote {}", out.path.display());
    if out.backed_up {
        println!("  (previous values backed up beside it as values.yaml.bak)");
    }
    println!();
    println!("1. Create the Secret the chart references — it holds the ONLY secret");
    println!("   material; the values file has none. A fresh SESSION_SECRET is below:");
    println!();
    println!("{}", indent(&out.secret_command));
    println!();
    println!("2. Install (or upgrade) the control plane:");
    println!();
    println!("{}", indent(&out.helm_command));
    println!();
    if !have("helm") {
        println!("Helm 3 is not installed here — get it to run the command above:");
        println!("  https://helm.sh/docs/intro/install/");
    }
    if !have("kubectl") {
        println!("kubectl is not installed here — you'll need it pointed at your cluster:");
        println!("  https://kubernetes.io/docs/tasks/tools/");
    }
    Ok(())
}

/// The testable core. `tty` is `None` for a non-interactive run; passing it in
/// (rather than opening it here) means a test can force that path with no risk of
/// blocking on a stray terminal.
pub fn run(opts: InitOptions, tty: Option<Tty>) -> Result<Outcome> {
    let release = trimmed(opts.release.clone()).unwrap_or_else(|| DEFAULT_RELEASE.into());
    let path = values_path(&opts.nook_dir, &release);

    // AC-4: a previous file for this release seeds every default.
    let existing = if path.exists() {
        parse_existing(
            &std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read {}", path.display()))?,
        )
    } else {
        Existing::default()
    };

    let mut tty = tty;

    // ── host ────────────────────────────────────────────────────────────────
    // The one field with no derived default. Without a terminal it MUST come
    // from --host (AC-2): picking a hostname on the operator's behalf would be a
    // guess baked into every URL the deployment then serves.
    let host = match &mut tty {
        Some(t) => {
            let seed = trimmed(opts.host.clone()).or_else(|| existing.host.clone());
            t.text("Ingress host that routes to this NookOS", seed.as_deref())?
        }
        None => trimmed(opts.host.clone()).ok_or_else(|| {
            anyhow::anyhow!(
                "no terminal to prompt on, and --host was not given.\n\n\
                 Re-run with the hostname that routes to NookOS, e.g.\n  \
                 nook k8s init --host nook.example.com\n\n\
                 Nothing was written."
            )
        })?,
    };
    let host = host.trim().trim_end_matches('/').to_string();

    // ── URLs (derived from host) ────────────────────────────────────────────
    let pbu_default = format!("https://{host}");
    let public_base_url = resolve(
        &mut tty,
        "Public base URL people open in a browser (PUBLIC_BASE_URL)",
        opts.public_base_url.clone(),
        existing.public_base_url.clone(),
        pbu_default,
    )?;
    let web_origin = resolve(
        &mut tty,
        "Allowed browser origin (WEB_ORIGIN)",
        opts.web_origin.clone(),
        existing.web_origin.clone(),
        public_base_url.clone(),
    )?;

    // ── secret name + ingress class ─────────────────────────────────────────
    let existing_secret = resolve(
        &mut tty,
        "Name of the Kubernetes Secret the chart reads",
        opts.secret_name.clone(),
        existing.existing_secret.clone(),
        DEFAULT_SECRET.into(),
    )?;
    // Empty className ⇒ omit the key (cluster default). A `--ingress-class ""` is
    // a deliberate way to clear one a re-run would otherwise carry forward.
    let ingress_class = resolve_optional(
        &mut tty,
        "Ingress class name (blank for the cluster default)",
        opts.ingress_class.clone(),
        existing.ingress_class.clone(),
    )?;

    // ── agent mTLS listener ─────────────────────────────────────────────────
    let agent_seed = opts.agent || existing.agent_enabled.unwrap_or(false);
    let agent_enabled = match &mut tty {
        Some(t) => t.confirm(
            "Enable the agent mTLS listener so external nodes can join?",
            agent_seed,
        )?,
        None => agent_seed,
    };
    let (agent_public_url, agent_tls_secret) = if agent_enabled {
        let url = resolve_optional(
            &mut tty,
            "Agent listener public address (host:8081)",
            opts.agent_url.clone(),
            existing.agent_url.clone(),
        )?;
        let tls = resolve_optional(
            &mut tty,
            "Name of the TLS Secret holding the agent cert",
            opts.agent_tls_secret.clone(),
            existing.agent_tls_secret.clone(),
        )?;
        (url, tls)
    } else {
        (None, None)
    };

    // ── chart version (non-values metadata) ─────────────────────────────────
    // flag > stored header > this binary's version. Empty ⇒ latest.
    let chart_seed = opts
        .chart_version
        .clone()
        .or_else(|| existing.chart_version.clone())
        .unwrap_or_else(|| opts.default_chart_version.clone());
    let chart_version = match &mut tty {
        Some(t) => {
            // Seed with a concrete version so Enter pins the matching chart;
            // clearing to "latest" is the `--chart-version ''` path.
            let seed = if chart_seed.trim().is_empty() {
                opts.default_chart_version.clone()
            } else {
                chart_seed.clone()
            };
            t.text("Chart version to pin (helm --version)", Some(&seed))?
        }
        None => chart_seed,
    };

    let namespace = resolve(
        &mut tty,
        "Kubernetes namespace",
        opts.namespace.clone(),
        existing.namespace.clone(),
        DEFAULT_NAMESPACE.into(),
    )?;

    let values = Values {
        existing_secret,
        public_base_url,
        web_origin,
        ingress_host: host,
        ingress_class,
        agent_enabled,
        agent_public_url,
        agent_tls_secret,
    };
    let meta = Meta {
        release,
        namespace,
        chart_version: chart_version.trim().to_string(),
    };

    // ── write (create dirs, back up any prior file) ─────────────────────────
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let backed_up = if path.exists() {
        let bak = path.with_file_name("values.yaml.bak");
        std::fs::copy(&path, &bak)
            .with_context(|| format!("cannot back up {} to {}", path.display(), bak.display()))?;
        true
    } else {
        false
    };
    let text = render_values(&values, &meta);
    std::fs::write(&path, &text).with_context(|| format!("cannot write {}", path.display()))?;

    // Fresh secret, printed only (NG-2). DATABASE_URL is a placeholder — the
    // wizard neither knows nor stores the real database credentials.
    let secret_command = secret_command(&values, &meta, &session_secret());
    let helm_command = helm_command(&meta, &path);

    Ok(Outcome {
        path,
        secret_command,
        helm_command,
        backed_up,
    })
}

fn values_path(nook_dir: &Path, release: &str) -> PathBuf {
    nook_dir.join("k8s").join(release).join("values.yaml")
}

/// A required scalar: flag > stored > derived, prompted (seeded) with a TTY.
fn resolve(
    tty: &mut Option<Tty>,
    question: &str,
    flag: Option<String>,
    stored: Option<String>,
    derived: String,
) -> Result<String> {
    let seed = trimmed(flag)
        .or_else(|| stored.and_then(some_trimmed))
        .unwrap_or(derived);
    match tty {
        Some(t) => t.text(question, Some(&seed)),
        None => Ok(seed),
    }
}

/// An optional scalar. An explicit flag wins, including an empty one, which
/// clears a value a re-run would otherwise carry forward. Interactively, Enter
/// keeps the stored seed and typing replaces it; a fresh run with a blank answer
/// yields `None` (the key is omitted).
fn resolve_optional(
    tty: &mut Option<Tty>,
    question: &str,
    flag: Option<String>,
    stored: Option<String>,
) -> Result<Option<String>> {
    if let Some(f) = flag {
        return Ok(some_trimmed(f));
    }
    let seed = stored.and_then(some_trimmed);
    match tty {
        Some(t) => Ok(t.optional(question)?.or(seed)),
        None => Ok(seed),
    }
}

/// 32 random bytes as lowercase hex — the same shape the server wizard uses for
/// `SESSION_SECRET`.
fn session_secret() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn have(cmd: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {cmd} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Render the values file, header first (AC-4 machine-readable metadata).
fn render_values(v: &Values, m: &Meta) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "# nook-k8s-init release={} namespace={} chart-version={}\n",
        m.release, m.namespace, m.chart_version
    ));
    s.push_str("# NookOS control plane — Helm values written by `nook k8s init`.\n");
    s.push_str("# A curated subset of charts/nook-control/values.yaml. Nothing here is a\n");
    s.push_str("# secret: the chart consumes ONE Kubernetes Secret, named below, which you\n");
    s.push_str("# create yourself (see the kubectl line this command printed).\n");
    s.push('\n');
    s.push_str(&format!("existingSecret: {}\n", v.existing_secret));
    s.push('\n');
    s.push_str("config:\n");
    s.push_str(&format!("  publicBaseUrl: {}\n", v.public_base_url));
    s.push_str(&format!("  webOrigin: {}\n", v.web_origin));
    s.push('\n');
    s.push_str("ingress:\n");
    s.push_str(&format!("  host: {}\n", v.ingress_host));
    if let Some(c) = &v.ingress_class {
        s.push_str(&format!("  className: {c}\n"));
    }
    s.push('\n');
    s.push_str("# Agent mTLS listener (:8081) — how EXTERNAL nodes join a cluster-hosted\n");
    s.push_str("# control plane. See the chart README, \"Agent mTLS listener\".\n");
    s.push_str("agent:\n");
    s.push_str(&format!("  enabled: {}\n", v.agent_enabled));
    if let Some(u) = &v.agent_public_url {
        s.push_str(&format!("  publicUrl: {u}\n"));
    }
    if let Some(ts) = &v.agent_tls_secret {
        s.push_str(&format!("  tlsSecret: {ts}\n"));
    }
    s
}

/// The `kubectl create secret` line (AC-5): the operator's Secret, named to match
/// `existingSecret`, carrying a placeholder DATABASE_URL and the fresh secret.
fn secret_command(v: &Values, m: &Meta, session_secret: &str) -> String {
    format!(
        "kubectl create secret generic {secret} -n {ns} \\\n  \
         --from-literal=DATABASE_URL='postgres://USER:PASSWORD@HOST:5432/nook' \\\n  \
         --from-literal=SESSION_SECRET='{session_secret}'",
        secret = v.existing_secret,
        ns = m.namespace,
    )
}

/// The `helm install` line (AC-5). `--version` appears only when a version is
/// pinned; an empty chart version means "latest published chart".
fn helm_command(m: &Meta, path: &Path) -> String {
    let mut s = format!("helm install {release} {CHART}", release = m.release);
    if !m.chart_version.trim().is_empty() {
        s.push_str(&format!(" --version {}", m.chart_version));
    }
    s.push_str(&format!(
        " -n {ns} -f {path}",
        ns = m.namespace,
        path = path.display()
    ));
    s
}

fn indent(block: &str) -> String {
    block
        .lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn trimmed(v: Option<String>) -> Option<String> {
    v.and_then(some_trimmed)
}

fn some_trimmed(s: String) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Defaults recovered from a previously written file (AC-4).
#[derive(Default)]
struct Existing {
    release: Option<String>,
    namespace: Option<String>,
    chart_version: Option<String>,
    existing_secret: Option<String>,
    public_base_url: Option<String>,
    web_origin: Option<String>,
    host: Option<String>,
    ingress_class: Option<String>,
    agent_enabled: Option<bool>,
    agent_url: Option<String>,
    agent_tls_secret: Option<String>,
}

/// Parse back what a prior run wrote. The format is one we control — a flat set
/// of uniquely-named keys plus a header line — so a targeted scan is exact and
/// needs no YAML dependency (the repo has none; `generate.rs` hand-builds too).
fn parse_existing(text: &str) -> Existing {
    let mut e = Existing::default();
    for line in text.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("# nook-k8s-init") {
            for tok in rest.split_whitespace() {
                if let Some((k, val)) = tok.split_once('=') {
                    match k {
                        "release" => e.release = some_trimmed(val.into()),
                        "namespace" => e.namespace = some_trimmed(val.into()),
                        // May be recorded empty (an explicit "latest"); keep that
                        // distinction so a re-run does not silently re-pin.
                        "chart-version" => e.chart_version = Some(val.to_string()),
                        _ => {}
                    }
                }
            }
        }
    }
    e.existing_secret = scalar(text, "existingSecret");
    e.public_base_url = scalar(text, "publicBaseUrl");
    e.web_origin = scalar(text, "webOrigin");
    e.host = scalar(text, "host");
    e.ingress_class = scalar(text, "className");
    e.agent_enabled = scalar(text, "enabled").map(|v| v == "true");
    e.agent_url = scalar(text, "publicUrl");
    e.agent_tls_secret = scalar(text, "tlsSecret");
    e
}

/// Find `key: value` on a non-comment line, tolerant of indentation and quotes.
/// The emitted keys are all distinct names, so a flat search is unambiguous.
fn scalar(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with('#') {
            continue;
        }
        if let Some(rest) = t.strip_prefix(key) {
            if let Some(val) = rest.trim_start().strip_prefix(':') {
                return some_trimmed(val.trim().trim_matches('"').into());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_opts(dir: &Path) -> InitOptions {
        InitOptions {
            release: None,
            namespace: None,
            host: None,
            public_base_url: None,
            web_origin: None,
            ingress_class: None,
            secret_name: None,
            agent: false,
            agent_url: None,
            agent_tls_secret: None,
            chart_version: None,
            default_chart_version: "9.9.9".into(),
            nook_dir: dir.to_path_buf(),
        }
    }

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "nook-k8s-{}-{:p}",
            std::process::id(),
            &std::process::id() as *const _
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn non_interactive_writes_expected_yaml_with_derived_defaults() {
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                ..base_opts(&dir)
            },
            None,
        )
        .unwrap();

        // Written to ~/.nook/k8s/nook/values.yaml (release defaults to "nook").
        assert_eq!(out.path, dir.join("k8s/nook/values.yaml"));
        assert!(out.path.exists());
        let text = std::fs::read_to_string(&out.path).unwrap();

        // Contract keys present, with derived defaults (AC-1, AC-3).
        assert!(text.contains("existingSecret: nook-control-secrets"));
        assert!(text.contains("  publicBaseUrl: https://nook.example.com"));
        assert!(text.contains("  webOrigin: https://nook.example.com"));
        assert!(text.contains("  host: nook.example.com"));
        assert!(text.contains("agent:"));
        assert!(text.contains("  enabled: false"));
        // Empty ingress class ⇒ key omitted.
        assert!(!text.contains("className"));
        assert!(!out.backed_up);
    }

    /// Pull the generated secret back out of the printed kubectl line.
    fn secret_in(command: &str) -> String {
        let start = command.find("SESSION_SECRET='").unwrap() + "SESSION_SECRET='".len();
        let rest = &command[start..];
        rest[..rest.find('\'').unwrap()].to_string()
    }

    #[test]
    fn emitted_values_carry_no_secret_material() {
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                ..base_opts(&dir)
            },
            None,
        )
        .unwrap();
        let text = std::fs::read_to_string(&out.path).unwrap();
        let secret = secret_in(&out.secret_command);

        // NG-2: the fresh secret is printed, never written to the file.
        assert_eq!(secret.len(), 64);
        assert!(secret.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!text.contains(&secret));
        assert!(!text.to_lowercase().contains("session_secret"));
        assert!(!text.to_lowercase().contains("database_url"));
        // The secret line carries the fresh secret and a DATABASE_URL placeholder.
        assert!(out.secret_command.contains("DATABASE_URL='postgres://"));
        assert!(out.secret_command.contains("-n nook"));
    }

    #[test]
    fn missing_host_without_tty_errors_and_writes_nothing() {
        let dir = tmp();
        let err = run(base_opts(&dir), None).unwrap_err();
        assert!(err.to_string().contains("--host"));
        // AC-2: nothing written.
        assert!(!dir.join("k8s/nook/values.yaml").exists());
    }

    #[test]
    fn flags_map_to_values_and_class_is_included_when_set() {
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("apps.example.org".into()),
                public_base_url: Some("https://nook.apps.example.org".into()),
                web_origin: Some("https://web.apps.example.org".into()),
                ingress_class: Some("nginx".into()),
                secret_name: Some("my-secrets".into()),
                agent: true,
                agent_url: Some("agent.example.org:8081".into()),
                agent_tls_secret: Some("agent-tls".into()),
                chart_version: Some("1.2.3".into()),
                ..base_opts(&dir)
            },
            None,
        )
        .unwrap();
        let t = std::fs::read_to_string(&out.path).unwrap();
        assert!(t.contains("existingSecret: my-secrets"));
        assert!(t.contains("  publicBaseUrl: https://nook.apps.example.org"));
        assert!(t.contains("  webOrigin: https://web.apps.example.org"));
        assert!(t.contains("  className: nginx"));
        assert!(t.contains("  enabled: true"));
        assert!(t.contains("  publicUrl: agent.example.org:8081"));
        assert!(t.contains("  tlsSecret: agent-tls"));
        // Pinned version → --version present in the helm line.
        assert!(out.helm_command.contains("--version 1.2.3"));
        assert!(out.helm_command.contains(CHART));
        assert!(out
            .helm_command
            .ends_with(&format!("-f {}", out.path.display())));
    }

    #[test]
    fn blank_chart_version_omits_the_helm_version_flag() {
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                chart_version: Some(String::new()), // explicit "latest"
                ..base_opts(&dir)
            },
            None,
        )
        .unwrap();
        assert!(!out.helm_command.contains("--version"));
        assert!(out.helm_command.contains("chart"));
    }

    #[test]
    fn default_chart_version_is_the_binary_version() {
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                ..base_opts(&dir) // default_chart_version = "9.9.9"
            },
            None,
        )
        .unwrap();
        assert!(out.helm_command.contains("--version 9.9.9"));
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(text.contains("chart-version=9.9.9"));
    }

    #[test]
    fn rerun_prefills_from_the_existing_file_and_backs_it_up() {
        let dir = tmp();
        // First run pins a version, a namespace and an ingress class.
        run(
            InitOptions {
                host: Some("first.example.com".into()),
                namespace: Some("team-a".into()),
                ingress_class: Some("nginx".into()),
                chart_version: Some("1.0.0".into()),
                ..base_opts(&dir)
            },
            None,
        )
        .unwrap();

        // Re-run non-interactively giving ONLY --host (AC-2 requires it with no
        // TTY); namespace, class and chart version must survive from the file.
        let out = run(
            InitOptions {
                host: Some("second.example.com".into()),
                ..base_opts(&dir)
            },
            None,
        )
        .unwrap();

        assert!(out.backed_up);
        let bak = out.path.with_file_name("values.yaml.bak");
        assert!(bak.exists());
        assert!(std::fs::read_to_string(&bak)
            .unwrap()
            .contains("first.example.com"));

        let t = std::fs::read_to_string(&out.path).unwrap();
        assert!(t.contains("  host: second.example.com")); // the new flag won
        assert!(t.contains("  className: nginx")); // survived
        assert!(t.contains("chart-version=1.0.0")); // survived (header)
        assert!(t.contains("namespace=team-a")); // survived (header)
        assert!(out.helm_command.contains("--version 1.0.0"));
        assert!(out.helm_command.contains("-n team-a"));
    }

    #[test]
    fn two_releases_keep_independent_files() {
        let dir = tmp();
        run(
            InitOptions {
                host: Some("prod.example.com".into()),
                ..base_opts(&dir)
            },
            None,
        )
        .unwrap();
        run(
            InitOptions {
                release: Some("staging".into()),
                host: Some("staging.example.com".into()),
                ..base_opts(&dir)
            },
            None,
        )
        .unwrap();

        let prod = std::fs::read_to_string(dir.join("k8s/nook/values.yaml")).unwrap();
        let staging = std::fs::read_to_string(dir.join("k8s/staging/values.yaml")).unwrap();
        assert!(prod.contains("prod.example.com"));
        assert!(!prod.contains("staging.example.com"));
        assert!(staging.contains("staging.example.com"));
        // The first file was not touched by the second run.
        assert!(!dir.join("k8s/nook/values.yaml.bak").exists());
    }

    #[test]
    fn session_secret_is_random_hex() {
        let a = session_secret();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, session_secret());
    }

    #[test]
    fn parse_roundtrips_what_render_wrote() {
        let v = Values {
            existing_secret: "s".into(),
            public_base_url: "https://h".into(),
            web_origin: "https://w".into(),
            ingress_host: "h".into(),
            ingress_class: Some("nginx".into()),
            agent_enabled: true,
            agent_public_url: Some("a:8081".into()),
            agent_tls_secret: Some("tls".into()),
        };
        let m = Meta {
            release: "r".into(),
            namespace: "ns".into(),
            chart_version: "1.2.3".into(),
        };
        let e = parse_existing(&render_values(&v, &m));
        assert_eq!(e.release.as_deref(), Some("r"));
        assert_eq!(e.namespace.as_deref(), Some("ns"));
        assert_eq!(e.chart_version.as_deref(), Some("1.2.3"));
        assert_eq!(e.existing_secret.as_deref(), Some("s"));
        assert_eq!(e.public_base_url.as_deref(), Some("https://h"));
        assert_eq!(e.web_origin.as_deref(), Some("https://w"));
        assert_eq!(e.host.as_deref(), Some("h"));
        assert_eq!(e.ingress_class.as_deref(), Some("nginx"));
        assert_eq!(e.agent_enabled, Some(true));
        assert_eq!(e.agent_url.as_deref(), Some("a:8081"));
        assert_eq!(e.agent_tls_secret.as_deref(), Some("tls"));
    }
}
