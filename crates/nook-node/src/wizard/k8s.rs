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
/// The OIDC scopes the chart ships as its default (AC-3).
const DEFAULT_SCOPES: &str = "openid profile email";

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
    /// Force the Advanced path on every multi-field branch without a TTY (AC-1):
    /// prompt-parity for automation. Per-field flags below work with or without it.
    pub advanced: bool,
    /// `production` (default) or `dev` (AC-4). Anything but `production` is dev,
    /// which turns the dev-login hatch ON — see [`dev_login_note`] (MAIN-671).
    pub app_env: Option<String>,
    /// RUST_LOG: a bare level or a full EnvFilter directive (AC-6). Default `info`.
    pub log_level: Option<String>,
    /// Turn on the OIDC branch (AC-3). Any `oidc_*` field below also implies it.
    pub oidc: bool,
    pub oidc_issuer: Option<String>,
    pub oidc_client_id: Option<String>,
    /// Secret: printed in the `kubectl create secret` line, never written (NG-4).
    pub oidc_client_secret: Option<String>,
    pub oidc_scopes: Option<String>,
    pub oidc_device_client_id: Option<String>,
    /// Only needed when the issuer's discovery doc does not advertise one (AC-3).
    pub oidc_device_authorization_endpoint: Option<String>,
    /// The operator's Giphy API key (MAIN-171 AC-6) → the printed secret command
    /// only, never written (NG-4). Absent leaves chat GIF-less, which is the
    /// default and a fully working deployment.
    pub giphy_key: Option<String>,
    /// Wire `secretKeys.giphyKey` without supplying the key here — for a re-run
    /// against a Secret that already carries it.
    pub giphy: bool,
    /// `capture` | `smtp` | `postmark` (AC-5). Any `mail_*` field also implies the
    /// Advanced mail path. Absent/`capture` leaves the chart default (delivers nothing).
    pub mail_provider: Option<String>,
    pub mail_from: Option<String>,
    /// Secret: the SMTP password or Postmark token, printed only (NG-4).
    pub mail_token: Option<String>,
    pub mail_send_enabled: bool,
    pub mail_notifications_enabled: bool,
    pub mail_max_per_month: Option<String>,
    pub mail_max_per_day: Option<String>,
    pub mail_smtp_host: Option<String>,
    pub mail_smtp_port: Option<String>,
    pub mail_smtp_tls: Option<String>,
    pub mail_smtp_username: Option<String>,
    pub mail_postmark_api_url: Option<String>,
    /// Work queue provider (MAIN-153): `database` (default) | `redis` | `sqs`.
    /// Any queue/worker field below can imply the branch, mirroring mail.
    pub queue_provider: Option<String>,
    /// Secret: the Redis URL, printed only (NG-4). provider=redis.
    pub redis_url: Option<String>,
    /// Optional Redis list name KEDA watches; omitted leaves the chart default.
    pub redis_list_name: Option<String>,
    /// Non-secret SQS queue URL (provider=sqs).
    pub sqs_queue_url: Option<String>,
    /// AWS region (provider=sqs).
    pub sqs_region: Option<String>,
    /// SQS credential mode: `irsa` (default, no secret) | `secret`.
    pub sqs_credentials_mode: Option<String>,
    /// Secret: AWS access key id, printed only (NG-4). sqs + credentialsMode=secret.
    pub aws_access_key_id: Option<String>,
    /// Secret: AWS secret access key, printed only (NG-4). sqs + credentialsMode=secret.
    pub aws_secret_access_key: Option<String>,
    /// Deploy the queue worker (`worker.enabled`). Implied by any worker/KEDA field.
    pub worker: bool,
    /// Worker replica count (`worker.replicas`). Default 1 when the worker is on.
    pub worker_replicas: Option<String>,
    /// `NOOK_WORK_TYPES` allow-list (`worker.workTypes`); empty = every type.
    pub worker_work_types: Option<String>,
    /// Autoscale the worker with KEDA (`autoscaling.keda.enabled`). Implies --worker.
    pub keda: bool,
    pub keda_min_replicas: Option<String>,
    pub keda_max_replicas: Option<String>,
    /// The binary's own build version — the default pin (AC-3). Injected by main
    /// so tests do not depend on the crate version.
    pub default_chart_version: String,
    /// The `~/.nook` base. Injected so tests point it at a tempdir instead of
    /// mutating `$HOME` (which would race across parallel tests).
    pub nook_dir: PathBuf,
}

/// The result of probing `<issuer>/.well-known/openid-configuration` (AC-3).
/// Injected into [`run`] so a test can hand it a canned document instead of a
/// network. `Unreachable` covers a failed fetch or an unparseable body — the
/// wizard warns and lets the operator proceed either way.
pub enum DiscoveryProbe {
    Reachable {
        device_authorization_endpoint: Option<String>,
        /// Whether the IdP will register a client for us: it advertises a
        /// `registration_endpoint` AND offers public clients (MAIN-651). Both
        /// halves are required — a registration endpoint that only issues
        /// confidential clients cannot help, because the control plane refuses
        /// a registration that comes back with a secret.
        registers_clients: bool,
    },
    Unreachable,
}

/// How an issuer is probed. The real one does a blocking HTTP GET; tests pass a
/// closure returning a fixed [`DiscoveryProbe`].
pub type Discover<'a> = dyn Fn(&str) -> DiscoveryProbe + 'a;

/// The curated values the file carries.
struct Values {
    existing_secret: String,
    public_base_url: String,
    web_origin: String,
    /// `dev` or `production` (AC-4).
    app_env: String,
    /// The dev-login hatch. Always `false` when `app_env` is `production` (AC-4).
    auth_dev_mode: bool,
    /// RUST_LOG level or EnvFilter (AC-6). Empty ⇒ key omitted (control-plane default).
    log_level: String,
    ingress_host: String,
    /// `None` ⇒ the `className` key is omitted (cluster default, AC-3).
    ingress_class: Option<String>,
    agent_enabled: bool,
    agent_public_url: Option<String>,
    agent_tls_secret: Option<String>,
    /// `None` when OIDC is off; the config keys are omitted entirely (AC-3).
    oidc: Option<OidcValues>,
    /// `None`/`capture` ⇒ no mail keys (chart default). Some ⇒ the mail block (AC-5).
    mail: Option<MailValues>,
    /// `None` ⇒ the `database` default (chart default) ⇒ no `queue` block.
    /// Some ⇒ a `redis`/`sqs` queue block (MAIN-153).
    queue: Option<QueueValues>,
    /// `None` ⇒ worker off (chart default) ⇒ no `worker`/`autoscaling` blocks.
    /// Some ⇒ the worker block, carrying its optional KEDA autoscaling (MAIN-153).
    worker: Option<WorkerValues>,
    /// Extra env injected via `controlPlane.extraEnv` — currently only a manual
    /// `OIDC_DEVICE_AUTHORIZATION_ENDPOINT` when discovery does not advertise one
    /// (AC-3). Kept general so the escape hatch is reusable. `(name, value)`.
    extra_env: Vec<(String, String)>,
    /// Secret KEY names the chart must read from the Secret, so the env is wired
    /// (chart omits them when blank). `(valuesKey, secretKey)` — e.g.
    /// `("oidcClientSecret", "OIDC_CLIENT_SECRET")`.
    secret_keys: Vec<(&'static str, &'static str)>,
}

/// The non-secret half of an OIDC config (the client secret never lands here — it
/// goes to the printed secret command only, NG-4).
struct OidcValues {
    issuer_url: String,
    client_id: String,
    redirect_url: String,
    scopes: String,
    device_client_id: String,
}

/// The non-secret half of a mail config (the SMTP password / Postmark token go to
/// the secret command only, NG-4).
struct MailValues {
    provider: String,
    from: String,
    send_enabled: bool,
    notifications_enabled: bool,
    max_per_month: Option<String>,
    max_per_day: Option<String>,
    smtp_host: Option<String>,
    smtp_port: Option<String>,
    smtp_tls: Option<String>,
    smtp_username: Option<String>,
    postmark_api_url: Option<String>,
}

/// The non-secret half of a queue config (the Redis URL / AWS keys go to the
/// secret command only, NG-4). `provider` is always `redis` or `sqs` here —
/// `database` is the chart default and emits no block at all.
struct QueueValues {
    provider: String,
    /// Optional Redis list name (provider=redis); `None` ⇒ chart default.
    redis_list_name: Option<String>,
    /// provider=sqs fields.
    sqs_queue_url: Option<String>,
    sqs_region: Option<String>,
    /// `irsa` | `secret` (provider=sqs).
    sqs_credentials_mode: Option<String>,
}

/// The worker deployment + its optional KEDA autoscaling (MAIN-153).
struct WorkerValues {
    replicas: String,
    /// `NOOK_WORK_TYPES` allow-list; `None` ⇒ every type (key omitted).
    work_types: Option<String>,
    /// `None` ⇒ static `worker.replicas`; Some ⇒ an `autoscaling.keda` block.
    keda: Option<KedaValues>,
}

/// The KEDA autoscaling bounds (`autoscaling.keda`, MAIN-153).
struct KedaValues {
    min_replicas: String,
    max_replicas: String,
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
    /// The steps `--agent` needs and the chart cannot do for you: create the
    /// listener's TLS Secret, then fill in its public address once the
    /// LoadBalancer has one (MAIN-650).
    ///
    /// Empty when the agent listener is off. A field rather than a `println!`
    /// in `init` so a test can assert the hand-off actually says this — its
    /// absence is what made a fresh install mount a Secret nobody had created.
    pub agent_steps: Vec<String>,
    /// The database the chart does NOT create, named before the install rather
    /// than discovered from a CrashLoopBackOff (MAIN-650).
    pub database_note: String,
    /// What editing the Secret later requires. Helm cannot roll a Deployment on
    /// a change to a Secret it does not render, so this says the quiet part
    /// (MAIN-650).
    pub secret_change_note: String,
    /// What the dev-login hatch is, and — when this install pairs it with a
    /// public host — what that costs (MAIN-671). Never empty: an operator who
    /// took the safe default is still told the rule, because the values file
    /// they keep is the thing a later `--set` will edit.
    pub dev_login_note: String,
    /// Whether [`Self::dev_login_note`] is the WARNING shape rather than the
    /// reassuring one — the hatch on, and a host published. Carried as a fact
    /// rather than left for the caller to re-derive from the text, so the
    /// hand-off's banner and this run's decision cannot disagree.
    pub dev_login_exposed: bool,
}

/// CLI entry point: open a terminal if there is one, do the work, print the
/// hand-off. Always exits success once the file is written (NG-1).
pub fn init(opts: InitOptions) -> Result<()> {
    let out = run(opts, Tty::open(), &real_discovery)?;

    // The hand-off goes to stdout, not /dev/tty: it is the thing an operator
    // copies, so it must survive being piped or captured.
    println!();
    println!("✓ wrote {}", out.path.display());
    if out.backed_up {
        println!("  (previous values backed up beside it as values.yaml.bak)");
    }
    println!();
    println!("1. {}", out.database_note);
    println!();
    println!("2. Create the Secret the chart references — it holds the ONLY secret");
    println!("   material; the values file has none. A fresh SESSION_SECRET is below:");
    println!();
    println!("{}", indent(&out.secret_command));
    println!();
    let mut n = 3;
    for step in &out.agent_steps {
        println!("{n}. {step}");
        println!();
        n += 1;
    }
    println!("{n}. Install (or upgrade) the control plane:");
    println!();
    println!("{}", indent(&out.helm_command));
    println!();
    n += 1;
    println!("{n}. {}", out.secret_change_note);
    println!();
    n += 1;
    // Last, and banner-wrapped when it is the warning shape: an operator scans
    // the END of a hand-off for what to do next, and this is the one step whose
    // cost is paid by everyone who can reach the host (MAIN-671).
    if out.dev_login_exposed {
        println!("{}", "!".repeat(72));
    }
    println!("{n}. {}", out.dev_login_note);
    if out.dev_login_exposed {
        println!("{}", "!".repeat(72));
    }
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
pub fn run(opts: InitOptions, tty: Option<Tty>, discover: &Discover) -> Result<Outcome> {
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

    // ── app-env (dev vs prod) ───────────────────────────────────────────────
    // Default PRODUCTION (MAIN-671). It used to be dev, which made the hatch's
    // own gate — `AUTH_DEV_MODE && !production` — fire only for someone who
    // already knew to ask for it: every non-interactive run wrote a public
    // ingress host and an open `POST /api/v1/auth/dev-login` in the same file.
    // `authDevMode` is still derived, never prompted, and forced off for
    // production so the wizard cannot emit the combo the chart rejects.
    let app_env_seed = trimmed(opts.app_env.clone())
        .or_else(|| existing.app_env.clone())
        .map(|s| normalize_app_env(&s))
        .unwrap_or_else(|| "production".into());
    let app_env = match &mut tty {
        Some(t) => {
            let idx = t.choose(
                "Application environment",
                APP_ENV_CHOICES,
                app_env_choice_default(&app_env_seed),
            )?;
            APP_ENV_CHOICES[idx].0.to_string()
        }
        None => app_env_seed,
    };
    let auth_dev_mode = app_env != "production";

    // Said HERE, before the remaining questions, so an interactive operator
    // hears the cost of the answer they just gave while they can still change
    // it — not only in the hand-off after the file is written (AC-2/AC-3).
    let dev_login_note = dev_login_note(auth_dev_mode, &host);
    if auth_dev_mode && !host.is_empty() {
        warn(&mut tty, &dev_login_note);
    }

    // ── logging ─────────────────────────────────────────────────────────────
    // Recommended = pick a level (default info); Advanced = a full EnvFilter (AC-6).
    let log_seed = trimmed(opts.log_level.clone())
        .or_else(|| existing.log_level.clone())
        .unwrap_or_else(|| "info".into());
    let log_advanced = advanced_mode(&mut tty, opts.advanced, "logging")?;
    let log_level = match &mut tty {
        Some(t) if log_advanced => t.text(
            "RUST_LOG directive (e.g. nook_control=debug,info)",
            Some(&log_seed),
        )?,
        Some(t) => {
            const LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];
            let def = LEVELS.iter().position(|l| *l == log_seed).unwrap_or(2);
            let menu: Vec<(&str, &str)> = LEVELS.iter().map(|l| (*l, "")).collect();
            LEVELS[t.choose("Log level", &menu, def)?].to_string()
        }
        None => log_seed,
    };

    // ── OIDC single sign-on (skippable, AC-3) ───────────────────────────────
    let oidc_seed = opts.oidc || existing.oidc_enabled.unwrap_or(false);
    let oidc_on = match &mut tty {
        Some(t) => t.confirm("Configure OIDC single sign-on?", oidc_seed)?,
        None => oidc_seed,
    };
    let mut oidc_client_secret: Option<String> = None;
    let mut extra_env: Vec<(String, String)> = Vec::new();
    let oidc = if oidc_on {
        let advanced = advanced_mode(&mut tty, opts.advanced, "OIDC single sign-on")?;

        let issuer_url = resolve_required(
            &mut tty,
            "OIDC issuer URL (OIDC_ISSUER_URL)",
            "--oidc-issuer",
            opts.oidc_issuer.clone(),
            existing.oidc_issuer.clone(),
        )?;
        let issuer_url = issuer_url.trim().trim_end_matches('/').to_string();

        // Verify discovery. Reachability only (NG-3); a miss warns and proceeds.
        let discovery = discover(&issuer_url);
        let mut registers_clients = false;
        let advertises_device = match &discovery {
            DiscoveryProbe::Reachable {
                device_authorization_endpoint,
                registers_clients: registers,
            } => {
                registers_clients = *registers;
                device_authorization_endpoint.is_some()
            }
            DiscoveryProbe::Unreachable => {
                warn(
                    &mut tty,
                    &format!(
                        "could not verify {issuer_url}/.well-known/openid-configuration \
                         — proceeding on the values you give; fix the issuer if this is wrong."
                    ),
                );
                false
            }
        };

        // An IdP that registers clients makes this OPTIONAL: left blank, the
        // control plane registers a public client for itself at boot and
        // remembers it (MAIN-651). Required only where that cannot happen --
        // asking for an id the operator would have to go and create is exactly
        // the hand-work this install path is trying to delete.
        let client_id = if registers_clients {
            let given = resolve(
                &mut tty,
                "OIDC client ID (OIDC_CLIENT_ID) — blank to register one automatically",
                opts.oidc_client_id.clone(),
                existing.oidc_client_id.clone(),
                String::new(),
            )?;
            let given = given.trim().to_string();
            if given.is_empty() {
                note(
                    &mut tty,
                    "no client ID given — the control plane will register a public client                      at this IdP on first boot and remember it (RFC 7591).",
                );
            }
            given
        } else {
            resolve_required(
                &mut tty,
                "OIDC client ID (OIDC_CLIENT_ID)",
                "--oidc-client-id",
                opts.oidc_client_id.clone(),
                existing.oidc_client_id.clone(),
            )?
        };
        // Secret: printed only (NG-4). Interactively collected (AC-3); a
        // secret-less non-interactive run wires the key and leaves the operator to
        // put the value in the Secret themselves. Skipped entirely when the client
        // is to be registered: a registered client is PUBLIC and has no secret, so
        // prompting for one would ask for something that will never exist.
        if !client_id.is_empty() {
            oidc_client_secret = match &mut tty {
                Some(t) => Some(t.text("OIDC client secret (OIDC_CLIENT_SECRET)", None)?),
                None => some_trimmed(opts.oidc_client_secret.clone().unwrap_or_default()),
            };
        }
        // Scopes: prompted only on the Advanced path; Recommended keeps the default.
        let scopes = if advanced {
            resolve(
                &mut tty,
                "OIDC scopes",
                opts.oidc_scopes.clone(),
                existing.oidc_scopes.clone(),
                DEFAULT_SCOPES.into(),
            )?
        } else {
            trimmed(opts.oidc_scopes.clone())
                .or_else(|| existing.oidc_scopes.clone())
                .unwrap_or_else(|| DEFAULT_SCOPES.into())
        };
        // Redirect is derived deterministically from the public base URL (AC-2/AC-3).
        let redirect_url = format!(
            "{}/api/v1/auth/callback",
            public_base_url.trim_end_matches('/')
        );

        // Device login: always the (public) device client id; a manual
        // authorization endpoint only when discovery does not advertise one (AC-3).
        // Optional on the same condition as the client ID above (MAIN-651): the
        // control plane falls back to whichever client it resolved when none is
        // named, and a registered client is already public, which is the only
        // property the device grant needs of it. Where nothing will be
        // registered there is nothing to fall back to, so it stays required.
        let device_client_id = if registers_clients {
            resolve(
                &mut tty,
                "OIDC device client ID (OIDC_DEVICE_CLIENT_ID) — blank to reuse the main client",
                opts.oidc_device_client_id.clone(),
                existing.oidc_device_client_id.clone(),
                String::new(),
            )?
            .trim()
            .to_string()
        } else {
            resolve_required(
                &mut tty,
                "OIDC device (public) client ID (OIDC_DEVICE_CLIENT_ID)",
                "--oidc-device-client-id",
                opts.oidc_device_client_id.clone(),
                existing.oidc_device_client_id.clone(),
            )?
        };
        if !advertises_device {
            let endpoint = resolve_required(
                &mut tty,
                "Device authorization endpoint (OIDC_DEVICE_AUTHORIZATION_ENDPOINT)",
                "--oidc-device-authorization-endpoint",
                opts.oidc_device_authorization_endpoint.clone(),
                existing.device_auth_endpoint.clone(),
            )?;
            extra_env.push(("OIDC_DEVICE_AUTHORIZATION_ENDPOINT".into(), endpoint));
        }

        Some(OidcValues {
            issuer_url,
            client_id,
            redirect_url,
            scopes,
            device_client_id,
        })
    } else {
        None
    };

    // ── mail (optional, AC-5) ───────────────────────────────────────────────
    // Recommended = capture (nothing sent), no prompts; Advanced = a real provider.
    let mail_seed_advanced = opts.advanced
        || opts
            .mail_provider
            .as_deref()
            .is_some_and(|p| p != "capture")
        || existing
            .mail_provider
            .as_deref()
            .is_some_and(|p| p != "capture");
    let mail_advanced = advanced_mode(&mut tty, mail_seed_advanced, "outbound email")?;
    let mut mail_token: Option<String> = None;
    let mail = if mail_advanced {
        let provider_seed = trimmed(opts.mail_provider.clone())
            .or_else(|| existing.mail_provider.clone())
            .filter(|p| p == "smtp" || p == "postmark")
            .unwrap_or_else(|| "smtp".into());
        let provider = match &mut tty {
            Some(t) => {
                let idx = t.choose(
                    "Mail provider",
                    &[
                        ("smtp", "any SMTP relay"),
                        ("postmark", "Postmark transactional email"),
                    ],
                    if provider_seed == "postmark" { 1 } else { 0 },
                )?;
                if idx == 1 { "postmark" } else { "smtp" }.to_string()
            }
            None => provider_seed,
        };
        let from = resolve_required(
            &mut tty,
            "MAIL_FROM address (e.g. \"NookOS <no-reply@example.com>\")",
            "--mail-from",
            opts.mail_from.clone(),
            existing.mail_from.clone(),
        )?;
        // Secret (SMTP password or Postmark token): printed only (NG-4).
        mail_token = match &mut tty {
            Some(t) => t.optional(if provider == "postmark" {
                "Postmark server token (POSTMARK_TOKEN)"
            } else {
                "SMTP password (SMTP_PASSWORD)"
            })?,
            None => some_trimmed(opts.mail_token.clone().unwrap_or_default()),
        };
        let send_enabled = confirm(
            &mut tty,
            "Actually send mail (MAIL_SEND_ENABLED)?",
            opts.mail_send_enabled || existing.mail_send_enabled.unwrap_or(false),
        )?;
        let notifications_enabled = confirm(
            &mut tty,
            "Also send notification emails (MAIL_NOTIFICATIONS_ENABLED)?",
            opts.mail_notifications_enabled || existing.mail_notifications_enabled.unwrap_or(false),
        )?;
        let max_per_month = resolve_optional(
            &mut tty,
            "Cap on real sends per month (blank = uncapped)",
            opts.mail_max_per_month.clone(),
            existing.mail_max_per_month.clone(),
        )?;
        let max_per_day = resolve_optional(
            &mut tty,
            "Cap on real sends per day (blank = uncapped)",
            opts.mail_max_per_day.clone(),
            existing.mail_max_per_day.clone(),
        )?;
        let (smtp_host, smtp_port, smtp_tls, smtp_username, postmark_api_url) =
            if provider == "smtp" {
                (
                    resolve_optional(
                        &mut tty,
                        "SMTP host",
                        opts.mail_smtp_host.clone(),
                        existing.mail_smtp_host.clone(),
                    )?,
                    resolve_optional(
                        &mut tty,
                        "SMTP port",
                        opts.mail_smtp_port.clone(),
                        existing.mail_smtp_port.clone(),
                    )?,
                    resolve_optional(
                        &mut tty,
                        "SMTP TLS (none/starttls/implicit)",
                        opts.mail_smtp_tls.clone(),
                        existing.mail_smtp_tls.clone(),
                    )?,
                    resolve_optional(
                        &mut tty,
                        "SMTP username",
                        opts.mail_smtp_username.clone(),
                        existing.mail_smtp_username.clone(),
                    )?,
                    None,
                )
            } else {
                (
                    None,
                    None,
                    None,
                    None,
                    resolve_optional(
                        &mut tty,
                        "Postmark API URL (blank for the default)",
                        opts.mail_postmark_api_url.clone(),
                        existing.mail_postmark_api_url.clone(),
                    )?,
                )
            };
        Some(MailValues {
            provider,
            from,
            send_enabled,
            notifications_enabled,
            max_per_month,
            max_per_day,
            smtp_host,
            smtp_port,
            smtp_tls,
            smtp_username,
            postmark_api_url,
        })
    } else {
        None
    };

    // ── Giphy GIFs in chat (MAIN-171 AC-6) ──────────────────────────────────
    // Offered, never required (NG-3): the default answer is no, and no is a
    // complete deployment — chat simply has no GIF button. Nothing is generated,
    // because no key ships in this repo or its images; an operator brings their
    // own or does without.
    let giphy_seed =
        opts.giphy || opts.giphy_key.is_some() || existing.giphy_enabled.unwrap_or(false);
    let giphy_enabled = match &mut tty {
        Some(t) => {
            let on = t.confirm("Enable Giphy GIFs in chat? (optional)", giphy_seed)?;
            if on {
                t.say("  Create a free key at https://developers.giphy.com — \"Create an App\".");
            }
            on
        }
        None => giphy_seed,
    };
    // Secret: printed only (NG-4). A re-run against a Secret that already holds
    // the key answers this blank and still gets `secretKeys.giphyKey` wired.
    let giphy_key = if giphy_enabled {
        match &mut tty {
            Some(t) => t.optional("Giphy API key (NOOK_GIPHY_KEY)")?,
            None => some_trimmed(opts.giphy_key.clone().unwrap_or_default()),
        }
    } else {
        None
    };

    // ── work queue provider (MAIN-153) ──────────────────────────────────────
    // Recommended = database (the chart default, zero extra infra) — no block.
    // redis/sqs each carry their own (mostly non-secret) config; the Redis URL
    // and the AWS keys are secrets, printed only (NG-4), never written.
    let queue_seed = trimmed(opts.queue_provider.clone())
        .or_else(|| existing.queue_provider.clone())
        .map(|p| normalize_queue_provider(&p))
        .unwrap_or_else(|| "database".into());
    let queue_provider = match &mut tty {
        Some(t) => {
            let def = match queue_seed.as_str() {
                "redis" => 1,
                "sqs" => 2,
                _ => 0,
            };
            let idx = t.choose(
                "Work queue provider",
                &[
                    ("database", "drain the Postgres work queue — no extra infra"),
                    ("redis", "drain a Redis list"),
                    ("sqs", "drain an AWS SQS queue"),
                ],
                def,
            )?;
            ["database", "redis", "sqs"][idx].to_string()
        }
        None => queue_seed,
    };
    let mut redis_url: Option<String> = None;
    let mut aws_access_key_id: Option<String> = None;
    let mut aws_secret_access_key: Option<String> = None;
    let queue = match queue_provider.as_str() {
        "redis" => {
            // Secret: printed only (NG-4). A secret-less non-interactive run wires
            // the key and leaves the operator to put the value in the Secret.
            redis_url = match &mut tty {
                Some(t) => t.optional("Redis URL (NOOK_REDIS_URL)")?,
                None => some_trimmed(opts.redis_url.clone().unwrap_or_default()),
            };
            let redis_list_name = resolve_optional(
                &mut tty,
                "Redis list KEDA watches (blank for the chart default)",
                opts.redis_list_name.clone(),
                existing.redis_list_name.clone(),
            )?;
            Some(QueueValues {
                provider: "redis".into(),
                redis_list_name,
                sqs_queue_url: None,
                sqs_region: None,
                sqs_credentials_mode: None,
            })
        }
        "sqs" => {
            let sqs_queue_url = resolve_optional(
                &mut tty,
                "SQS queue URL (NOOK_SQS_QUEUE_URL)",
                opts.sqs_queue_url.clone(),
                existing.sqs_queue_url.clone(),
            )?;
            let sqs_region = resolve_optional(
                &mut tty,
                "AWS region (NOOK_SQS_REGION)",
                opts.sqs_region.clone(),
                existing.sqs_region.clone(),
            )?;
            let creds_seed = trimmed(opts.sqs_credentials_mode.clone())
                .or_else(|| existing.sqs_credentials_mode.clone())
                .filter(|m| m == "irsa" || m == "secret")
                .unwrap_or_else(|| "irsa".into());
            let credentials_mode = match &mut tty {
                Some(t) => {
                    let idx = t.choose(
                        "AWS credentials",
                        &[
                            ("irsa", "pod IAM role (IRSA) — no secret"),
                            ("secret", "AWS access keys from the Secret"),
                        ],
                        if creds_seed == "secret" { 1 } else { 0 },
                    )?;
                    if idx == 1 { "secret" } else { "irsa" }.to_string()
                }
                None => creds_seed,
            };
            if credentials_mode == "secret" {
                // Both secrets: printed only (NG-4).
                aws_access_key_id = match &mut tty {
                    Some(t) => t.optional("AWS access key id (AWS_ACCESS_KEY_ID)")?,
                    None => some_trimmed(opts.aws_access_key_id.clone().unwrap_or_default()),
                };
                aws_secret_access_key = match &mut tty {
                    Some(t) => t.optional("AWS secret access key (AWS_SECRET_ACCESS_KEY)")?,
                    None => some_trimmed(opts.aws_secret_access_key.clone().unwrap_or_default()),
                };
            }
            Some(QueueValues {
                provider: "sqs".into(),
                redis_list_name: None,
                sqs_queue_url,
                sqs_region,
                sqs_credentials_mode: Some(credentials_mode),
            })
        }
        // database (or anything else) ⇒ the chart default, no block.
        _ => None,
    };

    // ── worker + autoscaling (MAIN-153) ─────────────────────────────────────
    // Off leaves the chart as it was (additive). KEDA autoscaling nests under an
    // enabled worker — scaling a worker that is not deployed is meaningless.
    let worker_seed = opts.worker
        || opts.worker_replicas.is_some()
        || opts.worker_work_types.is_some()
        || opts.keda
        || opts.keda_min_replicas.is_some()
        || opts.keda_max_replicas.is_some()
        || existing.worker_enabled.unwrap_or(false);
    let worker_enabled = match &mut tty {
        Some(t) => t.confirm("Deploy the queue worker?", worker_seed)?,
        None => worker_seed,
    };
    let worker = if worker_enabled {
        let replicas = resolve(
            &mut tty,
            "Worker replica count",
            opts.worker_replicas.clone(),
            existing.worker_replicas.clone(),
            "1".into(),
        )?;
        let work_types = resolve_optional(
            &mut tty,
            "NOOK_WORK_TYPES allow-list (comma-separated; blank = every type)",
            opts.worker_work_types.clone(),
            existing.worker_work_types.clone(),
        )?;
        let keda_seed = opts.keda
            || opts.keda_min_replicas.is_some()
            || opts.keda_max_replicas.is_some()
            || existing.keda_enabled.unwrap_or(false);
        let keda_on = match &mut tty {
            Some(t) => t.confirm(
                "Autoscale the worker with KEDA (KEDA must be installed in-cluster)?",
                keda_seed,
            )?,
            None => keda_seed,
        };
        let keda = if keda_on {
            let min_replicas = resolve(
                &mut tty,
                "KEDA minimum replicas",
                opts.keda_min_replicas.clone(),
                existing.keda_min_replicas.clone(),
                "1".into(),
            )?;
            let max_replicas = resolve(
                &mut tty,
                "KEDA maximum replicas",
                opts.keda_max_replicas.clone(),
                existing.keda_max_replicas.clone(),
                "10".into(),
            )?;
            Some(KedaValues {
                min_replicas: min_replicas.trim().to_string(),
                max_replicas: max_replicas.trim().to_string(),
            })
        } else {
            None
        };
        Some(WorkerValues {
            replicas: replicas.trim().to_string(),
            work_types,
            keda,
        })
    } else {
        None
    };

    // The chart wires a secret env only when its `secretKeys.*` name is non-empty,
    // so enabling OIDC/mail/redis/sqs must also set the key (independently of
    // whether THIS run captured the value — a re-run often relies on a Secret
    // already in place).
    let mut secret_keys: Vec<(&'static str, &'static str)> = Vec::new();
    if oidc.is_some() {
        secret_keys.push(("oidcClientSecret", "OIDC_CLIENT_SECRET"));
    }
    if let Some(m) = &mail {
        if m.provider == "postmark" {
            secret_keys.push(("postmarkToken", "POSTMARK_TOKEN"));
        } else {
            secret_keys.push(("smtpPassword", "SMTP_PASSWORD"));
        }
    }
    if giphy_enabled {
        secret_keys.push(("giphyKey", "NOOK_GIPHY_KEY"));
    }
    if let Some(q) = &queue {
        match q.provider.as_str() {
            "redis" => secret_keys.push(("redisUrl", "NOOK_REDIS_URL")),
            "sqs" if q.sqs_credentials_mode.as_deref() == Some("secret") => {
                secret_keys.push(("awsAccessKeyId", "AWS_ACCESS_KEY_ID"));
                secret_keys.push(("awsSecretAccessKey", "AWS_SECRET_ACCESS_KEY"));
            }
            _ => {}
        }
    }

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

    // Before the values below take ownership of these: the hand-off has to say
    // what `--agent` still needs, and it cannot read them once they have moved.
    let secret_change_note = secret_change_note(&release, &namespace);
    let agent_steps = agent_steps_for(
        agent_tls_secret.as_deref(),
        agent_public_url.as_deref(),
        &namespace,
    );

    let values = Values {
        existing_secret,
        public_base_url,
        web_origin,
        app_env,
        auth_dev_mode,
        log_level: log_level.trim().to_string(),
        ingress_host: host,
        ingress_class,
        agent_enabled,
        agent_public_url,
        agent_tls_secret,
        oidc,
        mail,
        queue,
        worker,
        extra_env,
        secret_keys,
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
    // wizard neither knows nor stores the real database credentials. The OIDC
    // client secret and mail token (if captured this run) join it here and only
    // here (NG-4).
    let secret_command = secret_command(
        &values,
        &meta,
        &session_secret(),
        oidc_client_secret.as_deref(),
        mail_token.as_deref(),
        giphy_key.as_deref(),
        QueueSecrets {
            redis_url: redis_url.as_deref(),
            aws_access_key_id: aws_access_key_id.as_deref(),
            aws_secret_access_key: aws_secret_access_key.as_deref(),
        },
    );
    let helm_command = helm_command(&meta, &path);

    Ok(Outcome {
        path,
        secret_command,
        helm_command,
        backed_up,
        agent_steps,
        database_note: database_note(),
        secret_change_note,
        dev_login_exposed: values.auth_dev_mode && !values.ingress_host.is_empty(),
        dev_login_note,
    })
}

/// The two application environments, in the order the prompt offers them
/// (MAIN-671). Production is FIRST as well as the default: a menu whose safe
/// answer is second reads as the alternative to the one above it, and this is
/// the question a hurried operator answers by pressing return.
const APP_ENV_CHOICES: &[(&str, &str)] = &[
    ("production", "real deployment — the dev-login hatch is OFF"),
    (
        "dev",
        "trying NookOS out — dev-login hatch ON: anyone who can reach it \
         signs in as anyone",
    ),
];

/// Which entry the prompt pre-selects. Anything that is not `production` is
/// dev, matching [`normalize_app_env`], so a re-run over a dev values file
/// still offers dev rather than silently flipping an install's environment.
fn app_env_choice_default(seed: &str) -> usize {
    if normalize_app_env(seed) == "production" {
        0
    } else {
        1
    }
}

/// What the dev-login hatch is, and what it costs on this install (MAIN-671).
///
/// Two shapes, because both are worth saying. With the hatch OFF it states the
/// rule — the values file is kept, and a later `--set config.authDevMode=true`
/// is exactly the edit this note exists to pre-empt. With the hatch ON *and* a
/// host published it names the exposure, the endpoint, and who ends up owning
/// the deployment; the PAIRING is the defect, so a dev install binding no
/// public host gets the ordinary rule and no siren.
fn dev_login_note(auth_dev_mode: bool, ingress_host: &str) -> String {
    let host = ingress_host.trim();
    if !auth_dev_mode {
        return [
            "The dev-login hatch (AUTH_DEV_MODE / config.authDevMode) is OFF, which",
            "   is what this values file says. It must NEVER be on where the control",
            "   plane is reachable: POST /api/v1/auth/dev-login signs any caller in as",
            "   any email, creating the user when the email is unknown, and the first",
            "   user on a fresh deployment is granted operator.",
        ]
        .join("\n");
    }
    if host.is_empty() {
        return [
            "The dev-login hatch (AUTH_DEV_MODE / config.authDevMode) is ON, because",
            "   this is a dev install. It must NEVER be on where the control plane is",
            "   reachable: POST /api/v1/auth/dev-login signs any caller in as any",
            "   email. Re-run with --app-env production before publishing a host.",
        ]
        .join("\n");
    }
    [
        "INSECURE — this install publishes a host AND leaves the dev-login".to_string(),
        "   hatch open. Anyone on the internet who can reach".to_string(),
        String::new(),
        format!("     POST https://{host}/api/v1/auth/dev-login"),
        String::new(),
        "   is signed in as any email they name — the user is CREATED when the".to_string(),
        "   email is unknown — and the first user on a fresh deployment is granted".to_string(),
        "   operator. The first caller owns the deployment.".to_string(),
        String::new(),
        "   Re-run with --app-env production (the default) unless this host is".to_string(),
        "   unreachable from anywhere you would not hand the deployment to.".to_string(),
    ]
    .join("\n")
}

/// The database step (MAIN-650 AC-6).
///
/// The chart brings no Postgres: `existingSecret` carries a `DATABASE_URL` and
/// that is the whole contract. An operator who has only ever run the compose
/// stack has never had to create one, so the omission surfaced as a control
/// plane that installed cleanly and then crash-looped on connect.
fn database_note() -> String {
    [
        "The chart does NOT create a database. Before installing, have a",
        "   PostgreSQL instance and a database that already EXISTS — the control",
        "   plane migrates a database, it does not create one. Put its URL in the",
        "   Secret above as DATABASE_URL:",
        "",
        "     postgres://USER:PASSWORD@HOST:5432/nook?sslmode=require",
        "",
        "   Managed Postgres (DigitalOcean, RDS, Cloud SQL) generally REQUIRES",
        "   TLS: without sslmode=require the connection is refused, and the error",
        "   names the handshake rather than the missing parameter.",
    ]
    .join("\n")
}

/// Editing the Secret afterwards (MAIN-650 AC-5).
///
/// The Deployments carry a checksum over the ConfigMap, so config changes roll
/// the pods by themselves. A Secret cannot work that way here: `existingSecret`
/// is created and owned by the operator, outside the chart, so Helm never
/// renders it and has nothing to hash. Rather than pretend otherwise, the
/// hand-off says what to run.
fn secret_change_note(release: &str, namespace: &str) -> String {
    [
        "Editing the Secret later does NOT restart anything. The chart rolls the",
        "   pods when its own config changes, but the Secret is yours and Helm",
        "   never sees it — so after changing a value in it, roll them yourself:",
        "",
        &format!(
            "     kubectl rollout restart deploy -n {namespace} \\\n       -l app.kubernetes.io/instance={release}"
        ),
    ]
    .join("\n")
}

/// What `--agent` still needs from a person, in order (MAIN-650).
///
/// The chart references `agent.tlsSecret` and mounts it; nothing creates it, and
/// the hand-off used to say nothing at all — so an install that followed the
/// printed steps exactly produced a pod mounting a Secret that did not exist.
///
/// `publicUrl` is the other half: it is baked into join tokens, so it must be
/// set BEFORE install, and it is the LoadBalancer's address, which does not
/// exist UNTIL after install. That ordering cannot be designed away here, so it
/// is stated instead.
fn agent_steps_for(
    tls_secret: Option<&str>,
    public_url: Option<&str>,
    namespace: &str,
) -> Vec<String> {
    let Some(secret) = tls_secret.map(str::trim).filter(|s| !s.is_empty()) else {
        return Vec::new();
    };

    let mut steps = vec![[
        "Create the agent listener's TLS Secret — the chart MOUNTS it and does",
        "   not create it, so an install without this mounts a Secret that does",
        "   not exist:",
        "",
        "     openssl req -x509 -newkey rsa:2048 -nodes -days 365 \\",
        "       -keyout agent.key -out agent.crt -subj \"/CN=nook-agent\"",
        &format!("     kubectl create secret tls {secret} -n {namespace} \\"),
        "       --cert=agent.crt --key=agent.key",
    ]
    .join("\n")];

    // The listener is self-signed, so a joining node pins it by fingerprint or
    // trusts nothing (MAIN-650 AC-1). The chart's NOTES.txt has always said so,
    // which is precisely why it was missing here — the knowledge existed
    // somewhere the person following THESE steps never reads.
    //
    // Read off the local file rather than back out of the Secret: the step
    // above just wrote it, it needs no cluster round trip, and it sidesteps
    // `-o jsonpath='{.data.tls.crt}'` — where the unescaped `.` in the key
    // silently selects nothing and hands `base64 -d` an empty string.
    steps.push(
        [
            "Read the certificate's fingerprint. The listener is self-signed, so",
            "   each node pins THIS value with --server-fingerprint; without it a",
            "   node has nothing to authenticate the control plane against:",
            "",
            "     openssl x509 -in agent.crt -outform der | sha256sum | cut -d' ' -f1",
            "",
            "   Keep it beside the join token — `nook enroll` wants both.",
        ]
        .join("\n"),
    );

    // A public address that does not start with a digit is a name or a
    // placeholder, not an address the LoadBalancer handed out.
    let unresolved = public_url
        .map(str::trim)
        .is_none_or(|u| u.is_empty() || !u.starts_with(|c: char| c.is_ascii_digit()));
    if unresolved {
        steps.push(
            [
                "Fill in the agent's public address AFTER installing. It is baked",
                "   into join tokens so it cannot stay blank, and it is the",
                "   LoadBalancer's address, which does not exist until the Service",
                "   is created:",
                "",
                &format!(
                    "     kubectl -n {namespace} get svc -l app.kubernetes.io/component=agent \\"
                ),
                "       -o jsonpath='{.items[0].status.loadBalancer.ingress[0].ip}'",
                "",
                "   Then re-run this command with --agent-url <that-ip>:8081 and",
                "   `helm upgrade` using the regenerated values.",
            ]
            .join("\n"),
        );
    }
    steps
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

/// The Recommended/Advanced fork shown before a multi-field branch (AC-1). The
/// wording is one place so every branch offers the identical choice. Without a
/// TTY the `--advanced` flag (threaded in as `forced`) drives the same paths.
fn advanced_mode(tty: &mut Option<Tty>, forced: bool, what: &str) -> Result<bool> {
    match tty {
        Some(t) => Ok(t.choose(
            &format!("Configure {what}"),
            &[
                ("Recommended", "curated safe defaults, fewest questions"),
                ("Advanced", "prompt for every field"),
            ],
            0,
        )? == 1),
        None => Ok(forced),
    }
}

/// A required scalar with no derived default: flag > stored, prompted (required)
/// with a TTY. Without one, a missing value is a clear error naming the flag —
/// the branch cannot proceed on a guess.
fn resolve_required(
    tty: &mut Option<Tty>,
    question: &str,
    flag_name: &str,
    flag: Option<String>,
    stored: Option<String>,
) -> Result<String> {
    let seed = trimmed(flag).or_else(|| stored.and_then(some_trimmed));
    match tty {
        Some(t) => t.text(question, seed.as_deref()),
        None => seed.ok_or_else(|| {
            anyhow::anyhow!("{question} is required, but there is no terminal — pass {flag_name}.")
        }),
    }
}

/// A yes/no with a seed default. Without a TTY the seed stands (a flag having
/// already set it).
fn confirm(tty: &mut Option<Tty>, question: &str, seed: bool) -> Result<bool> {
    match tty {
        Some(t) => t.confirm(question, seed),
        None => Ok(seed),
    }
}

/// Print a warning to the terminal if there is one, else to stderr — so a
/// discovery miss is visible whether or not a person is watching (AC-3).
/// Say something that is not a problem. `warn`'s twin, kept separate so a
/// deliberate choice does not print with a warning's siren.
fn note(tty: &mut Option<Tty>, msg: &str) {
    match tty {
        Some(t) => t.say(&format!("• {msg}")),
        None => eprintln!("note: {msg}"),
    }
}

fn warn(tty: &mut Option<Tty>, msg: &str) {
    match tty {
        Some(t) => t.say(&format!("⚠ {msg}")),
        None => eprintln!("warning: {msg}"),
    }
}

/// Fold any spelling of "prod" to `production`; everything else is `dev` (AC-4).
fn normalize_app_env(v: &str) -> String {
    match v.trim().to_ascii_lowercase().as_str() {
        "production" | "prod" => "production".into(),
        _ => "dev".into(),
    }
}

/// Fold a queue-provider string to one of the three the chart accepts; anything
/// unrecognised (including a blank) falls back to `database` (MAIN-153).
fn normalize_queue_provider(v: &str) -> String {
    match v.trim().to_ascii_lowercase().as_str() {
        "redis" => "redis".into(),
        "sqs" => "sqs".into(),
        _ => "database".into(),
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

/// Fetch `<issuer>/.well-known/openid-configuration` and report whether it
/// advertises a `device_authorization_endpoint` (AC-3).
///
/// Runs on a fresh thread with its own current-thread runtime: `init` is called
/// from inside the async `main`, and building a runtime there would panic, while
/// enabling reqwest's blocking feature would pull a second HTTP stack in. This
/// keeps the one dependency the crate already has and never touches the ambient
/// runtime. A failure of any kind is `Unreachable` — the caller warns and lets
/// the operator proceed (NG-3: reachability only, no token exchange).
fn real_discovery(issuer: &str) -> DiscoveryProbe {
    let url = format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    );
    let fetched = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        rt.block_on(async {
            let resp = reqwest::Client::new()
                .get(&url)
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
                .ok()?;
            if !resp.status().is_success() {
                return None;
            }
            let doc: serde_json::Value = resp.json().await.ok()?;
            Some((
                doc.get("device_authorization_endpoint")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                registers_clients(&doc),
            ))
        })
    })
    .join()
    .ok()
    .flatten();
    match fetched {
        Some((device_authorization_endpoint, registers_clients)) => DiscoveryProbe::Reachable {
            device_authorization_endpoint,
            registers_clients,
        },
        None => DiscoveryProbe::Unreachable,
    }
}

/// Read a discovery document for the two things RFC 7591 registration needs.
///
/// Absence of `token_endpoint_auth_methods_supported` is NOT permission: RFC
/// 8414 §2 says an omitted field means `client_secret_basic`, so a provider
/// that says nothing is saying "confidential". Reading silence as consent here
/// would offer to register against an IdP that then hands back a secret, and the
/// control plane refuses those.
fn registers_clients(doc: &serde_json::Value) -> bool {
    let has_endpoint = doc
        .get("registration_endpoint")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty());
    let public_ok = doc
        .get("token_endpoint_auth_methods_supported")
        .and_then(|v| v.as_array())
        .is_some_and(|m| m.iter().any(|v| v.as_str() == Some("none")));
    has_endpoint && public_ok
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
    // Secret KEY names the chart reads from that Secret. Present only for the
    // features enabled; the values themselves live in the printed secret command.
    if !v.secret_keys.is_empty() {
        s.push_str("secretKeys:\n");
        for (values_key, secret_key) in &v.secret_keys {
            s.push_str(&format!("  {values_key}: {secret_key}\n"));
        }
    }
    s.push('\n');
    s.push_str("config:\n");
    s.push_str(&format!("  appEnv: {}\n", v.app_env));
    // Only ever emitted true — production forces it off (AC-4), and the chart
    // omits a false one anyway.
    if v.auth_dev_mode {
        s.push_str("  authDevMode: true\n");
    }
    if !v.log_level.is_empty() {
        s.push_str(&format!("  logLevel: {}\n", v.log_level));
    }
    s.push_str(&format!("  publicBaseUrl: {}\n", v.public_base_url));
    s.push_str(&format!("  webOrigin: {}\n", v.web_origin));
    if let Some(o) = &v.oidc {
        s.push_str("  oidc:\n");
        s.push_str(&format!("    issuerUrl: {}\n", o.issuer_url));
        // Omitted rather than written empty when the client is to be registered
        // (MAIN-651): a bare `clientId:` is YAML null, and the chart's own
        // default is the empty string. Saying nothing leaves the default alone.
        if !o.client_id.is_empty() {
            s.push_str(&format!("    clientId: {}\n", o.client_id));
        }
        s.push_str(&format!("    redirectUrl: {}\n", o.redirect_url));
        s.push_str(&format!("    scopes: \"{}\"\n", o.scopes));
        if !o.device_client_id.is_empty() {
            s.push_str(&format!("    deviceClientId: {}\n", o.device_client_id));
        }
    }
    if let Some(m) = &v.mail {
        s.push_str("  mail:\n");
        s.push_str(&format!("    provider: {}\n", m.provider));
        s.push_str(&format!("    from: \"{}\"\n", m.from));
        if m.send_enabled {
            s.push_str("    sendEnabled: true\n");
        }
        if m.notifications_enabled {
            s.push_str("    notificationsEnabled: true\n");
        }
        if let Some(x) = &m.max_per_month {
            s.push_str(&format!("    maxPerMonth: \"{x}\"\n"));
        }
        if let Some(x) = &m.max_per_day {
            s.push_str(&format!("    maxPerDay: \"{x}\"\n"));
        }
        if let Some(x) = &m.smtp_host {
            s.push_str(&format!("    smtpHost: {x}\n"));
        }
        if let Some(x) = &m.smtp_port {
            s.push_str(&format!("    smtpPort: \"{x}\"\n"));
        }
        if let Some(x) = &m.smtp_tls {
            s.push_str(&format!("    smtpTls: {x}\n"));
        }
        if let Some(x) = &m.smtp_username {
            s.push_str(&format!("    smtpUsername: {x}\n"));
        }
        if let Some(x) = &m.postmark_api_url {
            s.push_str(&format!("    postmarkApiUrl: {x}\n"));
        }
    }
    s.push('\n');
    // Extra env (e.g. a manual OIDC device authorization endpoint, AC-3): no chart
    // key exists for it, so it rides the chart's generic escape hatch (NG-1).
    if !v.extra_env.is_empty() {
        s.push_str("controlPlane:\n");
        s.push_str("  extraEnv:\n");
        for (name, value) in &v.extra_env {
            s.push_str(&format!("    - name: {name}\n"));
            s.push_str(&format!("      value: \"{value}\"\n"));
        }
        s.push('\n');
    }
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
    // ── work queue (MAIN-153) ────────────────────────────────────────────────
    // Only redis/sqs emit a block; database is the chart default. The Redis URL
    // and AWS keys are NOT here — they live in the printed secret command (NG-4).
    if let Some(q) = &v.queue {
        s.push('\n');
        s.push_str("queue:\n");
        s.push_str(&format!("  provider: {}\n", q.provider));
        if q.provider == "redis" {
            if let Some(ln) = &q.redis_list_name {
                s.push_str("  redis:\n");
                s.push_str(&format!("    listName: \"{ln}\"\n"));
            }
        }
        if q.provider == "sqs" {
            s.push_str("  sqs:\n");
            if let Some(u) = &q.sqs_queue_url {
                s.push_str(&format!("    queueUrl: \"{u}\"\n"));
            }
            if let Some(r) = &q.sqs_region {
                s.push_str(&format!("    region: \"{r}\"\n"));
            }
            if let Some(mode) = &q.sqs_credentials_mode {
                s.push_str(&format!("    credentialsMode: {mode}\n"));
            }
        }
    }
    // ── worker + KEDA autoscaling (MAIN-153) ─────────────────────────────────
    if let Some(w) = &v.worker {
        s.push('\n');
        s.push_str("worker:\n");
        s.push_str("  enabled: true\n");
        s.push_str(&format!("  replicas: {}\n", w.replicas));
        if let Some(wt) = &w.work_types {
            s.push_str(&format!("  workTypes: \"{wt}\"\n"));
        }
        if let Some(k) = &w.keda {
            s.push('\n');
            s.push_str("autoscaling:\n");
            s.push_str("  keda:\n");
            s.push_str("    enabled: true\n");
            s.push_str(&format!("    minReplicas: {}\n", k.min_replicas));
            s.push_str(&format!("    maxReplicas: {}\n", k.max_replicas));
        }
    }
    s
}

/// The `kubectl create secret` line: the operator's Secret, named to match
/// `existingSecret`, carrying a placeholder DATABASE_URL, the fresh session
/// secret, and — appended here and ONLY here (NG-4) — any OIDC client secret or
/// mail token captured this run. The literal key matches what `secretKeys.*`
/// points the chart at (`OIDC_CLIENT_SECRET` / `SMTP_PASSWORD` / `POSTMARK_TOKEN`).
fn secret_command(
    v: &Values,
    m: &Meta,
    session_secret: &str,
    oidc_client_secret: Option<&str>,
    mail_token: Option<&str>,
    giphy_key: Option<&str>,
    queue: QueueSecrets,
) -> String {
    let QueueSecrets {
        redis_url,
        aws_access_key_id,
        aws_secret_access_key,
    } = queue;
    let mut s = format!(
        "kubectl create secret generic {secret} -n {ns} \\\n  \
         --from-literal=DATABASE_URL='postgres://USER:PASSWORD@HOST:5432/nook' \\\n  \
         --from-literal=SESSION_SECRET='{session_secret}'",
        secret = v.existing_secret,
        ns = m.namespace,
    );
    if let Some(secret) = oidc_client_secret.and_then(some_trimmed_str) {
        s.push_str(&format!(
            " \\\n  --from-literal=OIDC_CLIENT_SECRET='{secret}'"
        ));
    }
    if let Some(token) = mail_token.and_then(some_trimmed_str) {
        // The key follows the provider the values file wired.
        let key = if v.mail.as_ref().is_some_and(|m| m.provider == "postmark") {
            "POSTMARK_TOKEN"
        } else {
            "SMTP_PASSWORD"
        };
        s.push_str(&format!(" \\\n  --from-literal={key}='{token}'"));
    }
    // Chat's Giphy key (MAIN-171). A secret only in the sense that it is the
    // operator's own credential — the control plane hands it to signed-in
    // browsers, which is what Giphy's web keys are for. It rides the same
    // never-written path as the rest (NG-4).
    if let Some(k) = giphy_key.and_then(some_trimmed_str) {
        s.push_str(&format!(" \\\n  --from-literal=NOOK_GIPHY_KEY='{k}'"));
    }
    // Queue secrets (MAIN-153): the Redis URL (provider=redis) and the AWS keys
    // (sqs + credentialsMode=secret) join here and ONLY here (NG-4). The literal
    // keys match what `secretKeys.*` points the chart at.
    if let Some(u) = redis_url.and_then(some_trimmed_str) {
        s.push_str(&format!(" \\\n  --from-literal=NOOK_REDIS_URL='{u}'"));
    }
    if let Some(k) = aws_access_key_id.and_then(some_trimmed_str) {
        s.push_str(&format!(" \\\n  --from-literal=AWS_ACCESS_KEY_ID='{k}'"));
    }
    if let Some(k) = aws_secret_access_key.and_then(some_trimmed_str) {
        s.push_str(&format!(
            " \\\n  --from-literal=AWS_SECRET_ACCESS_KEY='{k}'"
        ));
    }
    s
}

/// The queue secrets captured this run — printed only (NG-4). Grouped so
/// [`secret_command`] stays a readable, few-argument builder.
struct QueueSecrets<'a> {
    redis_url: Option<&'a str>,
    aws_access_key_id: Option<&'a str>,
    aws_secret_access_key: Option<&'a str>,
}

/// Trim, returning `None` for an all-whitespace string — the `&str` twin of
/// `some_trimmed`, for the optional secrets that arrive as `Option<&str>`.
fn some_trimmed_str(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
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

/// Defaults recovered from a previously written file (AC-4/AC-7).
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
    app_env: Option<String>,
    log_level: Option<String>,
    oidc_enabled: Option<bool>,
    oidc_issuer: Option<String>,
    oidc_client_id: Option<String>,
    oidc_scopes: Option<String>,
    oidc_device_client_id: Option<String>,
    device_auth_endpoint: Option<String>,
    giphy_enabled: Option<bool>,
    mail_provider: Option<String>,
    mail_from: Option<String>,
    mail_send_enabled: Option<bool>,
    mail_notifications_enabled: Option<bool>,
    mail_max_per_month: Option<String>,
    mail_max_per_day: Option<String>,
    mail_smtp_host: Option<String>,
    mail_smtp_port: Option<String>,
    mail_smtp_tls: Option<String>,
    mail_smtp_username: Option<String>,
    mail_postmark_api_url: Option<String>,
    queue_provider: Option<String>,
    redis_list_name: Option<String>,
    sqs_queue_url: Option<String>,
    sqs_region: Option<String>,
    sqs_credentials_mode: Option<String>,
    worker_enabled: Option<bool>,
    worker_replicas: Option<String>,
    worker_work_types: Option<String>,
    keda_enabled: Option<bool>,
    keda_min_replicas: Option<String>,
    keda_max_replicas: Option<String>,
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

    // App-env, logging (AC-7 round-trip).
    e.app_env = scalar(text, "appEnv");
    e.log_level = scalar(text, "logLevel");

    // OIDC — enabled iff an issuer was written; the client secret was never
    // stored (NG-4), so it is not among the recovered fields.
    e.oidc_issuer = scalar(text, "issuerUrl");
    e.oidc_enabled = Some(e.oidc_issuer.is_some());
    e.oidc_client_id = scalar(text, "clientId");
    e.oidc_scopes = scalar(text, "scopes");
    e.oidc_device_client_id = scalar(text, "deviceClientId");
    e.device_auth_endpoint = extra_env_value(text, "OIDC_DEVICE_AUTHORIZATION_ENDPOINT");

    // Giphy (MAIN-171) — the key was never stored (NG-4), so the only trace of a
    // previous "yes" is the secret KEY name the chart was pointed at.
    e.giphy_enabled = Some(nested_scalar(text, "secretKeys:", "giphyKey").is_some());

    // Mail — the token was never stored (NG-4). `provider` is a name shared with
    // the queue block, so it must be read scoped to `mail:` (a flat scan would
    // pick up whichever block came first).
    e.mail_provider = nested_scalar(text, "mail:", "provider");
    e.mail_from = scalar(text, "from");
    e.mail_send_enabled = scalar(text, "sendEnabled").map(|v| v == "true");
    e.mail_notifications_enabled = scalar(text, "notificationsEnabled").map(|v| v == "true");
    e.mail_max_per_month = scalar(text, "maxPerMonth");
    e.mail_max_per_day = scalar(text, "maxPerDay");
    e.mail_smtp_host = scalar(text, "smtpHost");
    e.mail_smtp_port = scalar(text, "smtpPort");
    e.mail_smtp_tls = scalar(text, "smtpTls");
    e.mail_smtp_username = scalar(text, "smtpUsername");
    e.mail_postmark_api_url = scalar(text, "postmarkApiUrl");

    // Queue (MAIN-153) — the Redis URL / AWS keys were never stored (NG-4).
    // `provider` is scoped to `queue:` (shared with mail); the rest are uniquely
    // named. Absent queue block ⇒ database default.
    e.queue_provider = nested_scalar(text, "queue:", "provider");
    e.redis_list_name = scalar(text, "listName");
    e.sqs_queue_url = scalar(text, "queueUrl");
    e.sqs_region = scalar(text, "region");
    e.sqs_credentials_mode = scalar(text, "credentialsMode");

    // Worker + KEDA (MAIN-153). `enabled` is scoped to its block (agent, worker
    // and keda all use the name); the replica counts are uniquely named.
    e.worker_enabled = nested_scalar(text, "worker:", "enabled").map(|v| v == "true");
    e.worker_replicas = nested_scalar(text, "worker:", "replicas");
    e.worker_work_types = scalar(text, "workTypes");
    e.keda_enabled = nested_scalar(text, "keda:", "enabled").map(|v| v == "true");
    e.keda_min_replicas = scalar(text, "minReplicas");
    e.keda_max_replicas = scalar(text, "maxReplicas");
    e
}

/// Read a `key:` nested inside the block introduced by `header` (a full trimmed
/// line such as `"mail:"` or `"queue:"`). A bare name shared across blocks
/// (`provider:` under both `mail:` and `queue:`, `enabled:` under `agent:`,
/// `worker:` and `keda:`) would resolve to whichever block a flat [`scalar`] scan
/// hit first, so those reads scope to their section. The scan runs from the
/// header to the first line indented no deeper than it (the block's end).
fn nested_scalar(text: &str, header: &str, key: &str) -> Option<String> {
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let t = line.trim_start();
        if t.starts_with('#') {
            continue;
        }
        if t == header {
            let header_indent = line.len() - t.len();
            for next in lines.by_ref() {
                let nt = next.trim_start();
                if nt.is_empty() || nt.starts_with('#') {
                    continue;
                }
                let next_indent = next.len() - nt.len();
                if next_indent <= header_indent {
                    break; // dedented back out of the block
                }
                if let Some(rest) = nt.strip_prefix(key) {
                    if let Some(val) = rest.strip_prefix(':') {
                        return some_trimmed(val.trim().trim_matches('"').into());
                    }
                }
            }
            return None;
        }
    }
    None
}

/// Recover an `extraEnv` value by its env name: find the `- name: NAME` line and
/// read the `value:` on the next non-blank line. The wizard writes the pair in
/// exactly that order, so a targeted two-line scan is enough (no YAML dep).
fn extra_env_value(text: &str, name: &str) -> Option<String> {
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let t = line.trim_start();
        if t.strip_prefix("- name:").map(str::trim) == Some(name) {
            for next in lines.by_ref() {
                let n = next.trim_start();
                if let Some(val) = n.strip_prefix("value:") {
                    return some_trimmed(val.trim().trim_matches('"').into());
                }
            }
        }
    }
    None
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

    /// MAIN-650: an install that followed the printed steps exactly produced a
    /// pod mounting a Secret nobody had created, because the hand-off never
    /// mentioned it. The steps are asserted here rather than eyeballed — their
    /// ABSENCE was the whole defect.
    /// MAIN-650 AC-6: the database is a prerequisite the chart cannot satisfy,
    /// and an install that does not mention it fails after Helm reports success.
    #[test]
    fn the_handoff_names_the_database_the_chart_does_not_create() {
        let note = super::database_note();
        assert!(note.contains("does NOT create a database"), "{note}");
        assert!(note.contains("DATABASE_URL"), "{note}");
        // A managed instance refuses a plaintext connection, and the error it
        // gives names the handshake rather than the missing parameter.
        assert!(note.contains("sslmode=require"), "{note}");
        // The control plane migrates a database; it does not create one.
        assert!(note.contains("EXISTS"), "{note}");
    }

    /// MAIN-650 AC-5: the ConfigMap checksum rolls the pods on a config change,
    /// but `existingSecret` is outside the chart and Helm has nothing to hash —
    /// so the hand-off has to say so rather than leave an edit looking applied.
    #[test]
    fn the_handoff_says_a_secret_edit_needs_a_restart() {
        let note = super::secret_change_note("nook", "nook-system");
        assert!(note.contains("does NOT restart anything"), "{note}");
        // Selector rather than a rendered name: the Deployment's name depends on
        // release/chart interplay, and a wrong literal is worse than no command.
        assert!(
            note.contains("kubectl rollout restart deploy -n nook-system"),
            "{note}"
        );
        assert!(note.contains("app.kubernetes.io/instance=nook"), "{note}");
    }

    #[test]
    fn the_agent_handoff_says_what_the_chart_cannot_do_for_you() {
        let steps =
            super::agent_steps_for(Some("nook-agent-tls"), Some("PLACEHOLDER:8081"), "nook");
        let all = steps.join("\n");

        // The Secret the chart mounts and does not create.
        assert!(
            all.contains("kubectl create secret tls nook-agent-tls -n nook"),
            "{all}"
        );
        assert!(all.contains("openssl req -x509"), "{all}");

        // AC-1's other half: a self-signed listener is only safe to join if the
        // node pins it, so the value it pins has to be printed HERE. It was
        // documented solely in the chart's NOTES.txt, which nobody following
        // these steps ever sees.
        assert!(all.contains("fingerprint"), "{all}");
        assert!(
            all.contains("openssl x509 -in agent.crt -outform der | sha256sum"),
            "{all}"
        );
        // Read off the file, never back out of the Secret. `-o jsonpath=` is
        // fine over the Service below, whose path has no dotted KEY in it — but
        // `{.data.tls.crt}` reads `tls` then `crt`, matches nothing, and hands
        // `base64 -d` an empty string without failing.
        assert!(!all.contains("{.data."), "{all}");

        // The ordering: the address is the LoadBalancer's, so it cannot be known
        // before the Service exists, yet it is baked into join tokens.
        assert!(all.contains("AFTER installing"), "{all}");
        assert!(all.contains("--agent-url"), "{all}");

        // A real address settles the ordering step; the Secret and its
        // fingerprint remain, because neither depends on the address.
        let settled =
            super::agent_steps_for(Some("nook-agent-tls"), Some("152.42.155.192:8081"), "nook");
        assert_eq!(settled.len(), 2, "{settled:?}");
        assert!(settled.join("\n").contains("fingerprint"), "{settled:?}");

        // The listener is off: the hand-off says nothing about it at all.
        assert!(super::agent_steps_for(None, None, "nook").is_empty());
        assert!(super::agent_steps_for(Some("  "), None, "nook").is_empty());
    }
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
            advanced: false,
            app_env: None,
            log_level: None,
            oidc: false,
            oidc_issuer: None,
            oidc_client_id: None,
            oidc_client_secret: None,
            oidc_scopes: None,
            oidc_device_client_id: None,
            oidc_device_authorization_endpoint: None,
            giphy: false,
            giphy_key: None,
            mail_provider: None,
            mail_from: None,
            mail_token: None,
            mail_send_enabled: false,
            mail_notifications_enabled: false,
            mail_max_per_month: None,
            mail_max_per_day: None,
            mail_smtp_host: None,
            mail_smtp_port: None,
            mail_smtp_tls: None,
            mail_smtp_username: None,
            mail_postmark_api_url: None,
            queue_provider: None,
            redis_url: None,
            redis_list_name: None,
            sqs_queue_url: None,
            sqs_region: None,
            sqs_credentials_mode: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            worker: false,
            worker_replicas: None,
            worker_work_types: None,
            keda: false,
            keda_min_replicas: None,
            keda_max_replicas: None,
            default_chart_version: "9.9.9".into(),
            nook_dir: dir.to_path_buf(),
        }
    }

    /// A discovery probe for tests that never enable OIDC — it must never be
    /// called, but the signature needs one.
    fn no_probe(_: &str) -> DiscoveryProbe {
        DiscoveryProbe::Unreachable
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
            &no_probe,
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
            &no_probe,
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
        let err = run(base_opts(&dir), None, &no_probe).unwrap_err();
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
            &no_probe,
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
            &no_probe,
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
            &no_probe,
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
            &no_probe,
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
            &no_probe,
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
            &no_probe,
        )
        .unwrap();
        run(
            InitOptions {
                release: Some("staging".into()),
                host: Some("staging.example.com".into()),
                ..base_opts(&dir)
            },
            None,
            &no_probe,
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

    // ── the MAIN-63 branches (non-interactive; per AC-1 the flag paths
    //    reproduce the interactive results) ──────────────────────────────────

    /// A discovery probe that advertises a device endpoint (or not), from an IdP
    /// that does not register clients — the shape most of these tests want.
    fn probe_advertising(advertised: bool) -> impl Fn(&str) -> DiscoveryProbe {
        move |_: &str| DiscoveryProbe::Reachable {
            device_authorization_endpoint: advertised.then(|| "https://id.example/device".into()),
            registers_clients: false,
        }
    }

    /// An IdP that will register a public client for us (MAIN-651). It
    /// advertises a device endpoint too, which is not incidental: a provider
    /// modern enough to implement RFC 7591 publishes RFC 8628 as well, and a
    /// fixture without one would test a combination that does not occur.
    fn probe_registering() -> impl Fn(&str) -> DiscoveryProbe {
        |_: &str| DiscoveryProbe::Reachable {
            device_authorization_endpoint: Some("https://id.example/device".into()),
            registers_clients: true,
        }
    }

    #[test]
    fn recommended_defaults_are_minimal_and_safe() {
        // No branch flags: production app-env (hatch off), info logging, no
        // OIDC, mail left at the chart default (capture) — nothing but the base
        // secret.
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                ..base_opts(&dir)
            },
            None,
            &no_probe,
        )
        .unwrap();
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(text.contains("  appEnv: production"));
        assert!(text.contains("  logLevel: info")); // default level (AC-6)
        assert!(!text.contains("oidc:")); // OIDC skipped (AC-3)
        assert!(!text.contains("mail:")); // capture ⇒ no mail block (AC-5)
        assert!(!text.contains("secretKeys:")); // no feature secrets wired
        assert!(!text.contains("controlPlane:")); // no extra env
                                                  // The secret command is unchanged from the base wizard.
        assert!(out.secret_command.contains("SESSION_SECRET='"));
        assert!(!out.secret_command.contains("OIDC_CLIENT_SECRET"));
        assert!(!out.secret_command.contains("SMTP_PASSWORD"));
        assert!(!out.secret_command.contains("NOOK_GIPHY_KEY")); // MAIN-171 NG-3
    }

    /// MAIN-171 AC-6/NG-3/NG-4: Giphy is offered, never required. Skipping it
    /// leaves no trace; taking it wires the secret KEY name in the values and
    /// puts the key itself only in the printed command.
    #[test]
    fn giphy_is_optional_and_its_key_is_never_written() {
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                giphy_key: Some("gk-live-abc".into()),
                ..base_opts(&dir)
            },
            None,
            &no_probe,
        )
        .unwrap();
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(text.contains("  giphyKey: NOOK_GIPHY_KEY"));
        assert!(out
            .secret_command
            .contains("--from-literal=NOOK_GIPHY_KEY='gk-live-abc'"));
        assert!(!text.contains("gk-live-abc"), "NG-4: never written to disk");

        // A re-run recovers the "yes" from the wired key name — the key itself
        // was never stored — so it keeps wiring the env even with no flag.
        let out2 = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                ..base_opts(&dir)
            },
            None,
            &no_probe,
        )
        .unwrap();
        let text2 = std::fs::read_to_string(&out2.path).unwrap();
        assert!(text2.contains("  giphyKey: NOOK_GIPHY_KEY"));
        // …and prints nothing for it, because this run captured no key.
        assert!(!out2.secret_command.contains("NOOK_GIPHY_KEY"));
    }

    /// `--giphy` without a key is the re-run shape: wire the chart to a Secret
    /// that already carries it, without re-typing the key.
    #[test]
    fn giphy_can_be_wired_without_supplying_the_key() {
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                giphy: true,
                ..base_opts(&dir)
            },
            None,
            &no_probe,
        )
        .unwrap();
        assert!(std::fs::read_to_string(&out.path)
            .unwrap()
            .contains("  giphyKey: NOOK_GIPHY_KEY"));
        assert!(!out.secret_command.contains("NOOK_GIPHY_KEY"));
    }

    /// MAIN-671 AC-1: the whole defect in one assertion. A non-interactive run
    /// with no `--app-env` used to write `appEnv: dev` + `authDevMode: true`
    /// beside the public host it was just given, so every generated install
    /// answered `POST /api/v1/auth/dev-login` from the internet.
    #[test]
    fn the_non_interactive_default_is_production_with_the_hatch_off() {
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                ..base_opts(&dir)
            },
            None,
            &no_probe,
        )
        .unwrap();
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(text.contains("  appEnv: production"), "{text}");
        assert!(!text.contains("authDevMode: true"), "{text}");
        assert!(!out.dev_login_exposed);
        // AC-6: the hand-off still says what the hatch is, because the file it
        // just wrote is the one a later --set edits.
        assert!(
            out.dev_login_note.contains("AUTH_DEV_MODE"),
            "{}",
            out.dev_login_note
        );
        assert!(
            out.dev_login_note.contains("is OFF"),
            "{}",
            out.dev_login_note
        );

        // Full OIDC does NOT quietly re-open it — configuring a real identity
        // provider was reproduced as having no effect on the hatch at all.
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                oidc: true,
                oidc_issuer: Some("https://id.example".into()),
                oidc_client_id: Some("nook".into()),
                oidc_client_secret: Some("shh".into()),
                ..base_opts(&dir)
            },
            None,
            &probe_registering(),
        )
        .unwrap();
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(text.contains("  appEnv: production"), "{text}");
        assert!(!text.contains("authDevMode: true"), "{text}");
    }

    /// MAIN-671 AC-2: the PAIRING is the defect, so the warning tracks the pair
    /// and not either half. Both combinations, over the function the wizard
    /// prints and the interactive path warns with.
    #[test]
    fn a_dev_install_with_a_published_host_is_warned_about_by_name() {
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                app_env: Some("dev".into()),
                ..base_opts(&dir)
            },
            None,
            &no_probe,
        )
        .unwrap();
        assert!(std::fs::read_to_string(&out.path)
            .unwrap()
            .contains("  authDevMode: true"));
        assert!(out.dev_login_exposed);
        let note = &out.dev_login_note;
        // The exposure, the exact endpoint, and who ends up owning the box.
        assert!(note.contains("INSECURE"), "{note}");
        assert!(
            note.contains("POST https://nook.example.com/api/v1/auth/dev-login"),
            "{note}"
        );
        assert!(note.contains("operator"), "{note}");
        assert!(note.contains("--app-env production"), "{note}");

        // Production with the same host: no warning, because there is nothing
        // open to warn about.
        assert!(!dev_login_note(false, "nook.example.com").contains("INSECURE"));

        // Dev with no published host stays supported and unsirened — it is the
        // pairing that is the defect, not the hatch (NG-1).
        let quiet = dev_login_note(true, "");
        assert!(!quiet.contains("INSECURE"), "{quiet}");
        assert!(quiet.contains("is ON"), "{quiet}");
        assert!(quiet.contains("--app-env production"), "{quiet}");
    }

    /// MAIN-671 AC-3: the interactive path makes the same statement. The prompt
    /// cannot be driven in a unit test — `Tty` is a handle on /dev/tty — so its
    /// two decisions are the pure functions it is built from: which entry is
    /// pre-selected, and what the entries say.
    #[test]
    fn the_prompt_defaults_to_production_and_says_what_dev_costs() {
        // Nothing stored and no flag ⇒ the seed is production ⇒ entry 0.
        assert_eq!(app_env_choice_default("production"), 0);
        assert_eq!(APP_ENV_CHOICES[0].0, "production");
        // A re-run over a dev values file still offers dev — the prompt states a
        // default, it does not silently flip a running install's environment.
        assert_eq!(app_env_choice_default("dev"), 1);
        assert_eq!(app_env_choice_default("staging"), 1);
        assert_eq!(APP_ENV_CHOICES[1].0, "dev");

        // And the menu says the cost in the line the operator reads before
        // choosing, not only in the warning after.
        assert!(
            APP_ENV_CHOICES[0].1.contains("OFF"),
            "{:?}",
            APP_ENV_CHOICES
        );
        let dev = APP_ENV_CHOICES[1].1;
        assert!(dev.contains("ON"), "{dev}");
        assert!(dev.contains("signs in as anyone"), "{dev}");

        // The warning the interactive path emits on a dev + host answer is the
        // one the hand-off prints; asserting it here is asserting both.
        let warned = dev_login_note(true, "nook.example.com");
        assert!(warned.contains("/api/v1/auth/dev-login"), "{warned}");
    }

    #[test]
    fn prod_app_env_never_emits_dev_mode() {
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                app_env: Some("production".into()),
                ..base_opts(&dir)
            },
            None,
            &no_probe,
        )
        .unwrap();
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(text.contains("  appEnv: production"));
        // AC-4: the production + dev-mode combo the chart rejects is never written.
        assert!(!text.contains("authDevMode"));
    }

    #[test]
    fn advanced_oidc_writes_config_derives_redirect_and_prints_only_the_secret() {
        // Discovery does NOT advertise a device endpoint ⇒ the manual one is
        // required and rides controlPlane.extraEnv (AC-3, NG-1).
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                oidc: true,
                oidc_issuer: Some("https://id.example/".into()),
                oidc_client_id: Some("confidential".into()),
                oidc_client_secret: Some("s3cr3t".into()),
                oidc_device_client_id: Some("public-device".into()),
                oidc_device_authorization_endpoint: Some("https://id.example/oauth/device".into()),
                ..base_opts(&dir)
            },
            None,
            &probe_advertising(false),
        )
        .unwrap();
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(text.contains("    issuerUrl: https://id.example")); // trailing / trimmed
        assert!(text.contains("    clientId: confidential"));
        // Redirect derived deterministically from the public base URL (AC-2/AC-3).
        assert!(text.contains("    redirectUrl: https://nook.example.com/api/v1/auth/callback"));
        assert!(text.contains("    scopes: \"openid profile email\"")); // recommended default
        assert!(text.contains("    deviceClientId: public-device"));
        // Manual device endpoint via the chart's generic escape hatch (NG-1).
        assert!(text.contains("controlPlane:"));
        assert!(text.contains("- name: OIDC_DEVICE_AUTHORIZATION_ENDPOINT"));
        assert!(text.contains("value: \"https://id.example/oauth/device\""));
        // secretKeys wires the env; the value is in the command, never on disk.
        assert!(text.contains("  oidcClientSecret: OIDC_CLIENT_SECRET"));
        assert!(out
            .secret_command
            .contains("--from-literal=OIDC_CLIENT_SECRET='s3cr3t'"));
        assert!(!text.contains("s3cr3t")); // NG-4
    }

    #[test]
    fn oidc_with_advertised_device_endpoint_needs_no_manual_one() {
        // Discovery advertises the endpoint ⇒ no manual prompt, no extraEnv, and
        // the missing --oidc-device-authorization-endpoint flag is not an error.
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                oidc: true,
                oidc_issuer: Some("https://id.example".into()),
                oidc_client_id: Some("confidential".into()),
                oidc_client_secret: Some("s3cr3t".into()),
                oidc_device_client_id: Some("public-device".into()),
                ..base_opts(&dir)
            },
            None,
            &probe_advertising(true),
        )
        .unwrap();
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(text.contains("    deviceClientId: public-device"));
        assert!(!text.contains("OIDC_DEVICE_AUTHORIZATION_ENDPOINT"));
        assert!(!text.contains("controlPlane:"));
    }

    #[test]
    fn advanced_mail_postmark_wires_token_secret_only_in_the_command() {
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                mail_provider: Some("postmark".into()),
                mail_from: Some("NookOS <no-reply@example.com>".into()),
                mail_token: Some("pm-token".into()),
                mail_send_enabled: true,
                ..base_opts(&dir)
            },
            None,
            &no_probe,
        )
        .unwrap();
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(text.contains("    provider: postmark"));
        assert!(text.contains("    from: \"NookOS <no-reply@example.com>\""));
        assert!(text.contains("    sendEnabled: true"));
        assert!(text.contains("  postmarkToken: POSTMARK_TOKEN"));
        assert!(out
            .secret_command
            .contains("--from-literal=POSTMARK_TOKEN='pm-token'"));
        assert!(!text.contains("pm-token")); // NG-4
        assert!(!out.secret_command.contains("SMTP_PASSWORD"));
    }

    #[test]
    fn advanced_mail_smtp_writes_the_smtp_block() {
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                mail_provider: Some("smtp".into()),
                mail_from: Some("NookOS <no-reply@example.com>".into()),
                mail_token: Some("smtp-pw".into()),
                mail_smtp_host: Some("smtp.example.com".into()),
                mail_smtp_port: Some("587".into()),
                mail_smtp_tls: Some("starttls".into()),
                mail_smtp_username: Some("mailer".into()),
                ..base_opts(&dir)
            },
            None,
            &no_probe,
        )
        .unwrap();
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(text.contains("    provider: smtp"));
        assert!(text.contains("    smtpHost: smtp.example.com"));
        assert!(text.contains("    smtpPort: \"587\""));
        assert!(text.contains("    smtpTls: starttls"));
        assert!(text.contains("    smtpUsername: mailer"));
        assert!(text.contains("  smtpPassword: SMTP_PASSWORD"));
        assert!(out
            .secret_command
            .contains("--from-literal=SMTP_PASSWORD='smtp-pw'"));
        assert!(!text.contains("smtp-pw")); // NG-4
    }

    #[test]
    fn rerun_prefills_the_new_fields_from_the_saved_file() {
        let dir = tmp();
        // First run configures everything via flags (discovery not advertising,
        // so a manual device endpoint is captured too).
        run(
            InitOptions {
                host: Some("nook.example.com".into()),
                app_env: Some("production".into()),
                log_level: Some("debug".into()),
                oidc: true,
                oidc_issuer: Some("https://id.example".into()),
                oidc_client_id: Some("confidential".into()),
                oidc_client_secret: Some("s3cr3t".into()),
                oidc_scopes: Some("openid email".into()),
                oidc_device_client_id: Some("public-device".into()),
                oidc_device_authorization_endpoint: Some("https://id.example/oauth/device".into()),
                mail_provider: Some("smtp".into()),
                mail_from: Some("NookOS <no-reply@example.com>".into()),
                mail_smtp_host: Some("smtp.example.com".into()),
                ..base_opts(&dir)
            },
            None,
            &probe_advertising(false),
        )
        .unwrap();

        // Re-run with ONLY --host. Every new non-secret field must survive.
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                ..base_opts(&dir)
            },
            None,
            &probe_advertising(false),
        )
        .unwrap();
        assert!(out.backed_up);
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(text.contains("  appEnv: production"));
        assert!(text.contains("  logLevel: debug"));
        assert!(text.contains("    issuerUrl: https://id.example"));
        assert!(text.contains("    clientId: confidential"));
        assert!(text.contains("    scopes: \"openid email\""));
        assert!(text.contains("    deviceClientId: public-device"));
        assert!(text.contains("value: \"https://id.example/oauth/device\""));
        assert!(text.contains("    provider: smtp"));
        assert!(text.contains("    smtpHost: smtp.example.com"));
        // The secret was never stored, so a bare re-run cannot re-print it (NG-4).
        assert!(!text.contains("s3cr3t"));
        assert!(!out.secret_command.contains("OIDC_CLIENT_SECRET")); // no value this run
                                                                     // …but the wiring survives, so the operator's existing Secret still works.
        assert!(text.contains("  oidcClientSecret: OIDC_CLIENT_SECRET"));
    }

    /// MAIN-651: an IdP that registers clients makes the client ID optional, so
    /// the install path stops asking for something the operator would have to go
    /// and create by hand.
    #[test]
    fn a_registering_idp_makes_the_client_id_optional() {
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                oidc: true,
                oidc_issuer: Some("https://id.example".into()),
                ..base_opts(&dir)
            },
            None,
            &probe_registering(),
        )
        .unwrap();
        let text = std::fs::read_to_string(&out.path).unwrap();

        // The OIDC block is written, and the id is left for the boot-time
        // registration to supply -- omitted, not an empty scalar (which is null).
        assert!(text.contains("issuerUrl: https://id.example"), "{text}");
        // Neither id is written: both are left for the boot-time registration,
        // and both are OMITTED rather than empty scalars (which are YAML null).
        assert!(!text.contains("clientId:"), "{text}");
        assert!(!text.contains("deviceClientId:"), "{text}");
        // A registered client is public, so no secret key is wired for it.
        assert!(
            !out.secret_command.contains("OIDC_CLIENT_SECRET"),
            "{}",
            out.secret_command
        );
    }

    /// The other half of the same rule: where the IdP will NOT register, the id
    /// is still required and the error names the flag. Silence about a client
    /// that cannot be obtained is how OIDC ends up degraded after a clean run.
    #[test]
    fn an_idp_that_does_not_register_still_requires_the_client_id() {
        let dir = tmp();
        let err = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                oidc: true,
                oidc_issuer: Some("https://id.example".into()),
                ..base_opts(&dir)
            },
            None,
            &probe_advertising(false),
        )
        .unwrap_err();
        assert!(err.to_string().contains("--oidc-client-id"), "{err}");
    }

    /// Both halves are required before registration is offered. RFC 8414 §2
    /// makes an omitted `token_endpoint_auth_methods_supported` mean
    /// `client_secret_basic`, so silence is confidential, not permissive.
    #[test]
    fn registration_needs_an_endpoint_and_a_public_client() {
        let both = serde_json::json!({
            "registration_endpoint": "https://id.example/reg",
            "token_endpoint_auth_methods_supported": ["client_secret_post", "none"],
        });
        assert!(super::registers_clients(&both));

        let confidential_only = serde_json::json!({
            "registration_endpoint": "https://id.example/reg",
            "token_endpoint_auth_methods_supported": ["client_secret_basic"],
        });
        assert!(!super::registers_clients(&confidential_only));

        // Endpoint, but the field omitted entirely: NOT permission.
        let silent = serde_json::json!({"registration_endpoint": "https://id.example/reg"});
        assert!(!super::registers_clients(&silent));

        // Public clients, but nowhere to register.
        let no_endpoint = serde_json::json!({"token_endpoint_auth_methods_supported": ["none"]});
        assert!(!super::registers_clients(&no_endpoint));
    }

    #[test]
    fn oidc_without_a_terminal_requires_the_issuer() {
        // OIDC on but no issuer and no TTY ⇒ a clear error naming the flag (AC-1).
        let dir = tmp();
        let err = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                oidc: true,
                ..base_opts(&dir)
            },
            None,
            &no_probe,
        )
        .unwrap_err();
        assert!(err.to_string().contains("--oidc-issuer"));
    }

    // ── the MAIN-153 branches: queue provider + worker/KEDA (AC-3) ───────────

    #[test]
    fn database_default_emits_no_queue_or_worker_block() {
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                ..base_opts(&dir)
            },
            None,
            &no_probe,
        )
        .unwrap();
        let text = std::fs::read_to_string(&out.path).unwrap();
        // database is the chart default ⇒ no queue/worker/autoscaling overrides.
        assert!(!text.contains("queue:"));
        assert!(!text.contains("worker:"));
        assert!(!text.contains("autoscaling:"));
        assert!(!text.contains("NOOK_REDIS_URL"));
        assert!(!out.secret_command.contains("NOOK_REDIS_URL"));
        assert!(!out.secret_command.contains("AWS_"));
    }

    #[test]
    fn redis_wires_the_secret_key_and_prints_the_url_only() {
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                queue_provider: Some("redis".into()),
                redis_url: Some("redis://:pw@redis:6379/0".into()),
                ..base_opts(&dir)
            },
            None,
            &no_probe,
        )
        .unwrap();
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(text.contains("queue:"));
        assert!(text.contains("  provider: redis"));
        // secretKeys wires the env; the URL itself is in the command, never on disk.
        assert!(text.contains("  redisUrl: NOOK_REDIS_URL"));
        assert!(out
            .secret_command
            .contains("--from-literal=NOOK_REDIS_URL='redis://:pw@redis:6379/0'"));
        // NG-4: the URL is printed, never written.
        assert!(!text.contains("redis://:pw@redis:6379/0"));
        // No list name given ⇒ the redis sub-block is omitted (chart default).
        assert!(!text.contains("listName"));
    }

    #[test]
    fn redis_list_name_is_emitted_when_set() {
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                queue_provider: Some("redis".into()),
                redis_list_name: Some("nook:jobs".into()),
                ..base_opts(&dir)
            },
            None,
            &no_probe,
        )
        .unwrap();
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(text.contains("  redis:"));
        assert!(text.contains("    listName: \"nook:jobs\""));
    }

    #[test]
    fn sqs_irsa_writes_the_url_and_region_and_no_aws_secret() {
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                queue_provider: Some("sqs".into()),
                sqs_queue_url: Some("https://sqs.us-east-1.amazonaws.com/1/q".into()),
                sqs_region: Some("us-east-1".into()),
                // credentials mode defaults to irsa
                ..base_opts(&dir)
            },
            None,
            &no_probe,
        )
        .unwrap();
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(text.contains("  provider: sqs"));
        assert!(text.contains("    queueUrl: \"https://sqs.us-east-1.amazonaws.com/1/q\""));
        assert!(text.contains("    region: \"us-east-1\""));
        assert!(text.contains("    credentialsMode: irsa"));
        // IRSA ⇒ no AWS secret keys wired, none in the command.
        assert!(!text.contains("awsAccessKeyId"));
        assert!(!out.secret_command.contains("AWS_ACCESS_KEY_ID"));
        assert!(!out.secret_command.contains("AWS_SECRET_ACCESS_KEY"));
    }

    #[test]
    fn sqs_secret_mode_wires_aws_keys_and_prints_them_only() {
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                queue_provider: Some("sqs".into()),
                sqs_queue_url: Some("https://sqs.eu-west-1.amazonaws.com/1/q".into()),
                sqs_region: Some("eu-west-1".into()),
                sqs_credentials_mode: Some("secret".into()),
                aws_access_key_id: Some("AKIAEXAMPLE".into()),
                aws_secret_access_key: Some("wJalrXUtnFEMI".into()),
                ..base_opts(&dir)
            },
            None,
            &no_probe,
        )
        .unwrap();
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(text.contains("    credentialsMode: secret"));
        assert!(text.contains("  awsAccessKeyId: AWS_ACCESS_KEY_ID"));
        assert!(text.contains("  awsSecretAccessKey: AWS_SECRET_ACCESS_KEY"));
        // Both AWS keys ride the command, never the file (NG-4).
        assert!(out
            .secret_command
            .contains("--from-literal=AWS_ACCESS_KEY_ID='AKIAEXAMPLE'"));
        assert!(out
            .secret_command
            .contains("--from-literal=AWS_SECRET_ACCESS_KEY='wJalrXUtnFEMI'"));
        assert!(!text.contains("AKIAEXAMPLE"));
        assert!(!text.contains("wJalrXUtnFEMI"));
    }

    #[test]
    fn worker_enabled_renders_replicas_and_work_types() {
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                worker: true,
                worker_replicas: Some("4".into()),
                worker_work_types: Some("build,review".into()),
                ..base_opts(&dir)
            },
            None,
            &no_probe,
        )
        .unwrap();
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(text.contains("worker:"));
        assert!(text.contains("  enabled: true"));
        assert!(text.contains("  replicas: 4"));
        assert!(text.contains("  workTypes: \"build,review\""));
        // No KEDA flag ⇒ no autoscaling block.
        assert!(!text.contains("autoscaling:"));
    }

    #[test]
    fn worker_defaults_to_one_replica_and_omits_work_types() {
        let dir = tmp();
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                worker: true,
                ..base_opts(&dir)
            },
            None,
            &no_probe,
        )
        .unwrap();
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(text.contains("  replicas: 1"));
        assert!(!text.contains("workTypes"));
    }

    #[test]
    fn keda_enabled_renders_min_and_max_and_implies_worker() {
        let dir = tmp();
        // Only KEDA flags: the worker must be turned on for KEDA to mean anything.
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                keda: true,
                keda_min_replicas: Some("2".into()),
                keda_max_replicas: Some("9".into()),
                ..base_opts(&dir)
            },
            None,
            &no_probe,
        )
        .unwrap();
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(text.contains("worker:"));
        assert!(text.contains("  enabled: true"));
        assert!(text.contains("autoscaling:"));
        assert!(text.contains("  keda:"));
        assert!(text.contains("    enabled: true"));
        assert!(text.contains("    minReplicas: 2"));
        assert!(text.contains("    maxReplicas: 9"));
    }

    #[test]
    fn rerun_prefills_queue_and_worker_from_the_saved_file() {
        let dir = tmp();
        run(
            InitOptions {
                host: Some("nook.example.com".into()),
                queue_provider: Some("sqs".into()),
                sqs_queue_url: Some("https://sqs.us-east-1.amazonaws.com/1/q".into()),
                sqs_region: Some("us-east-1".into()),
                sqs_credentials_mode: Some("secret".into()),
                aws_access_key_id: Some("AKIAEXAMPLE".into()),
                aws_secret_access_key: Some("shh".into()),
                worker: true,
                worker_replicas: Some("5".into()),
                worker_work_types: Some("build".into()),
                keda: true,
                keda_min_replicas: Some("3".into()),
                keda_max_replicas: Some("12".into()),
                ..base_opts(&dir)
            },
            None,
            &no_probe,
        )
        .unwrap();

        // Re-run with ONLY --host: every non-secret queue/worker field survives.
        let out = run(
            InitOptions {
                host: Some("nook.example.com".into()),
                ..base_opts(&dir)
            },
            None,
            &no_probe,
        )
        .unwrap();
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(text.contains("  provider: sqs"));
        assert!(text.contains("    region: \"us-east-1\""));
        assert!(text.contains("    credentialsMode: secret"));
        assert!(text.contains("  replicas: 5"));
        assert!(text.contains("  workTypes: \"build\""));
        assert!(text.contains("    minReplicas: 3"));
        assert!(text.contains("    maxReplicas: 12"));
        // secretKeys wiring survives so the operator's existing Secret still works…
        assert!(text.contains("  awsAccessKeyId: AWS_ACCESS_KEY_ID"));
        // …but the AWS secret values were never stored, so a bare re-run can't
        // re-print them (NG-4).
        assert!(!out.secret_command.contains("AKIAEXAMPLE"));
        assert!(!text.contains("AKIAEXAMPLE"));
    }

    #[test]
    fn parse_roundtrips_what_render_wrote() {
        let v = Values {
            existing_secret: "s".into(),
            public_base_url: "https://h".into(),
            web_origin: "https://w".into(),
            app_env: "dev".into(),
            auth_dev_mode: true,
            log_level: "debug".into(),
            ingress_host: "h".into(),
            ingress_class: Some("nginx".into()),
            agent_enabled: true,
            agent_public_url: Some("a:8081".into()),
            agent_tls_secret: Some("tls".into()),
            oidc: Some(OidcValues {
                issuer_url: "https://id.example".into(),
                client_id: "cid".into(),
                redirect_url: "https://h/api/v1/auth/callback".into(),
                scopes: "openid profile email".into(),
                device_client_id: "dcid".into(),
            }),
            mail: Some(MailValues {
                provider: "smtp".into(),
                from: "NookOS <no-reply@h>".into(),
                send_enabled: true,
                notifications_enabled: false,
                max_per_month: Some("1000".into()),
                max_per_day: None,
                smtp_host: Some("smtp.h".into()),
                smtp_port: Some("587".into()),
                smtp_tls: Some("starttls".into()),
                smtp_username: Some("mailer".into()),
                postmark_api_url: None,
            }),
            queue: Some(QueueValues {
                provider: "sqs".into(),
                redis_list_name: None,
                sqs_queue_url: Some("https://sqs.us-east-1.amazonaws.com/1/q".into()),
                sqs_region: Some("us-east-1".into()),
                sqs_credentials_mode: Some("secret".into()),
            }),
            worker: Some(WorkerValues {
                replicas: "3".into(),
                work_types: Some("build,review".into()),
                keda: Some(KedaValues {
                    min_replicas: "2".into(),
                    max_replicas: "8".into(),
                }),
            }),
            extra_env: vec![(
                "OIDC_DEVICE_AUTHORIZATION_ENDPOINT".into(),
                "https://id.example/device".into(),
            )],
            secret_keys: vec![
                ("oidcClientSecret", "OIDC_CLIENT_SECRET"),
                ("smtpPassword", "SMTP_PASSWORD"),
            ],
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
        // New fields round-trip (AC-7).
        assert_eq!(e.app_env.as_deref(), Some("dev"));
        assert_eq!(e.log_level.as_deref(), Some("debug"));
        assert_eq!(e.oidc_enabled, Some(true));
        assert_eq!(e.oidc_issuer.as_deref(), Some("https://id.example"));
        assert_eq!(e.oidc_client_id.as_deref(), Some("cid"));
        assert_eq!(e.oidc_scopes.as_deref(), Some("openid profile email"));
        assert_eq!(e.oidc_device_client_id.as_deref(), Some("dcid"));
        assert_eq!(
            e.device_auth_endpoint.as_deref(),
            Some("https://id.example/device")
        );
        assert_eq!(e.mail_provider.as_deref(), Some("smtp"));
        assert_eq!(e.mail_from.as_deref(), Some("NookOS <no-reply@h>"));
        assert_eq!(e.mail_send_enabled, Some(true));
        assert_eq!(e.mail_max_per_month.as_deref(), Some("1000"));
        assert_eq!(e.mail_smtp_host.as_deref(), Some("smtp.h"));
        assert_eq!(e.mail_smtp_port.as_deref(), Some("587"));
        assert_eq!(e.mail_smtp_tls.as_deref(), Some("starttls"));
        assert_eq!(e.mail_smtp_username.as_deref(), Some("mailer"));
        // Queue + worker + KEDA round-trip (MAIN-153). `provider` reads scoped to
        // its block, so the queue value is not shadowed by the mail one.
        assert_eq!(e.queue_provider.as_deref(), Some("sqs"));
        assert_eq!(
            e.sqs_queue_url.as_deref(),
            Some("https://sqs.us-east-1.amazonaws.com/1/q")
        );
        assert_eq!(e.sqs_region.as_deref(), Some("us-east-1"));
        assert_eq!(e.sqs_credentials_mode.as_deref(), Some("secret"));
        assert_eq!(e.worker_enabled, Some(true));
        assert_eq!(e.worker_replicas.as_deref(), Some("3"));
        assert_eq!(e.worker_work_types.as_deref(), Some("build,review"));
        assert_eq!(e.keda_enabled, Some(true));
        assert_eq!(e.keda_min_replicas.as_deref(), Some("2"));
        assert_eq!(e.keda_max_replicas.as_deref(), Some("8"));
    }
}
