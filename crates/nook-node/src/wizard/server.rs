//! `nook server init` — stand up a control plane.
//!
//! Deliberately generates its own secrets rather than shipping a template to
//! edit. `.env.example` carries a `SESSION_SECRET` of sixty-four zeroes, and a
//! deployment that keeps it works perfectly: it serves traffic and signs
//! sessions with a key published on GitHub. Nothing fails until someone
//! notices, so the installer never offers the opportunity.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::generate::{self, Deployment, Oidc, ServerAnswers};
use super::tty;

/// 32 random bytes as lowercase hex.
fn secret() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
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

/// `docker compose` (v2) or `docker-compose` (v1), or neither.
fn compose_cmd() -> Option<Vec<String>> {
    if std::process::Command::new("docker")
        .args(["compose", "version"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return Some(vec!["docker".into(), "compose".into()]);
    }
    have("docker-compose").then(|| vec!["docker-compose".into()])
}

pub struct InitOptions {
    pub dir: Option<PathBuf>,
    pub version: String,
    pub dry_run: bool,
}

pub fn init(opts: InitOptions) -> Result<()> {
    let mut t = tty::require("nook server init --dir /srv/nook   # then edit the generated .env")?;

    t.say("");
    t.say("  NookOS control plane");
    t.say("  Machines connect to this. It holds the database and the web UI.");
    t.say("");

    let dir = match opts.dir {
        Some(d) => d,
        None => PathBuf::from(t.text("Install directory", Some("/srv/nook"))?),
    };
    if dir.join(".env").exists() {
        // Regenerating secrets over a live deployment signs every user out and
        // makes every stored secret undecryptable. Never do it by accident.
        bail!(
            "{} already exists.\n\nThis directory already holds a control plane. Regenerating would \
             replace its\nSECRETS_KEY, which is what stored secrets are encrypted with — they would \
             become\nunreadable. To upgrade instead, edit the image tag in docker-compose.yml and \
             run\n`docker compose pull && docker compose up -d`.",
            dir.join(".env").display()
        );
    }

    let public_url = loop {
        let v = t.text(
            "Public URL people will open in a browser",
            Some("https://nook.example.com"),
        )?;
        if v.starts_with("http://") || v.starts_with("https://") {
            break v.trim_end_matches('/').to_string();
        }
        t.say("  Include the scheme, e.g. https://nook.example.com");
    };

    // ---- deployment mode
    let compose = compose_cmd();
    let modes: [(&str, &str); 4] = [
        (
            "Docker Compose",
            "Postgres, control plane and web UI together. Ports published directly.",
        ),
        (
            "Docker Compose behind Traefik",
            "Same, plus router labels — including the agent-port passthrough nodes need.",
        ),
        (
            "docker run",
            "Prints the commands. Needs a Postgres you already run — Compose brings its own.",
        ),
        (
            "systemd + native binary",
            "No containers. Needs a Postgres you already run — Compose brings its own.",
        ),
    ];
    let pick = t.choose("How should it run?", &modes, 0)?;
    let deployment = match pick {
        0 => Deployment::Compose,
        1 => Deployment::ComposeTraefik,
        2 => Deployment::DockerRun,
        _ => Deployment::Systemd,
    };

    if matches!(deployment, Deployment::Compose | Deployment::ComposeTraefik) && compose.is_none() {
        bail!(
            "Docker Compose is not installed.\n\nInstall Docker (https://docs.docker.com/engine/install/) \
             and run this again,\nor choose 'docker run' / 'systemd' instead."
        );
    }

    // ---- where nodes connect
    //
    // Its own name by default under Traefik, because passthrough routes on SNI
    // and cannot distinguish agent traffic from the API on a shared hostname.
    let default_agent = if deployment == Deployment::ComposeTraefik {
        let host = ServerAnswers::host_of(&public_url);
        format!("https://agent.{host}")
    } else {
        public_url.clone()
    };
    t.say("");
    t.say("Nodes do not connect through the same door as the browser: their TLS");
    t.say("must terminate in the control plane itself, so it can check each node's");
    t.say("certificate against the right tenant's CA.");
    let agent_url = t
        .text("URL for node connections", Some(&default_agent))?
        .trim_end_matches('/')
        .to_string();

    // ---- database
    let (postgres_password, database_url) = match deployment {
        Deployment::Compose | Deployment::ComposeTraefik => (Some(secret()), None),
        _ => {
            // This mode does not install Postgres, and saying so once here is
            // cheaper than someone discovering it from a crash loop. The schema
            // needs no action — `sqlx::migrate!` runs at startup — so the role
            // and database are genuinely the whole prerequisite.
            t.say("");
            t.say("This mode does not install Postgres. Point it at one you run.");
            t.say("If you need to create it:");
            t.say("");
            t.say("    CREATE ROLE nook LOGIN PASSWORD 'choose-something';");
            t.say("    CREATE DATABASE nook OWNER nook;");
            t.say("");
            t.say("Nothing further — the control plane migrates the schema on startup.");
            (None, Some(ask_database_url(&mut t)?))
        }
    };

    // ---- sign-in
    t.say("");
    let auth = t.choose(
        "How will people sign in?",
        &[
            ("OIDC now", "Any provider — Authentik, Keycloak, Auth0, Okta, Google."),
            (
                "Look around first",
                "Dev sign-in, no provider. NOT for production; the server refuses to start with it in production mode.",
            ),
        ],
        0,
    )?;
    let (oidc, dev_auth) = if auth == 0 {
        let issuer_url = t.text("OIDC issuer URL", None)?;
        let client_id = t.text("Client ID", None)?;
        let client_secret = t.text("Client secret", None)?;
        t.say("");
        t.say(&format!(
            "  Set the redirect URI at your provider to:\n    {}/api/v1/auth/callback",
            public_url
        ));
        (
            Some(Oidc {
                issuer_url,
                client_id,
                client_secret,
            }),
            false,
        )
    } else {
        (None, true)
    };

    let tenant_name = t.text("Organisation name", Some("default"))?;

    let answers = ServerAnswers {
        public_url: public_url.clone(),
        agent_url: agent_url.clone(),
        deployment,
        version: opts.version.clone(),
        postgres_password,
        database_url,
        session_secret: secret(),
        secrets_key: secret(),
        mcp_token: secret(),
        oidc,
        dev_auth,
        tenant_name,
    };

    // ---- write
    if opts.dry_run {
        t.say("");
        t.say("--- .env ---");
        t.say(&generate::env_file(&answers));
        if matches!(deployment, Deployment::Compose | Deployment::ComposeTraefik) {
            t.say("--- docker-compose.yml ---");
            t.say(&generate::compose_file(&answers));
        }
        t.say("(dry run — nothing written)");
        return Ok(());
    }

    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    write_private(&dir.join(".env"), &generate::env_file(&answers))?;
    t.say(&format!("✓ {} (0600)", dir.join(".env").display()));

    match deployment {
        Deployment::Compose | Deployment::ComposeTraefik => {
            std::fs::write(
                dir.join("docker-compose.yml"),
                generate::compose_file(&answers),
            )?;
            t.say(&format!("✓ {}", dir.join("docker-compose.yml").display()));
        }
        Deployment::DockerRun => {
            let p = dir.join("run-containers.sh");
            std::fs::write(&p, generate::docker_run_script(&answers))?;
            t.say(&format!("✓ {}", p.display()));
        }
        Deployment::Systemd => {}
    }

    // ---- the agent certificate, and its fingerprint
    let fingerprint = agent_certificate(&dir, &agent_url)?;
    t.say(&format!("✓ {}/agent-certs (0600 key)", dir.display()));

    // ---- bring it up
    let mut running = false;
    if let (Some(cmd), true) = (
        compose,
        matches!(deployment, Deployment::Compose | Deployment::ComposeTraefik),
    ) {
        if t.confirm("Start it now?", true)? {
            t.say("");
            let status = std::process::Command::new(&cmd[0])
                .args(&cmd[1..])
                .args(["up", "-d"])
                .current_dir(&dir)
                .status()?;
            running = status.success();
            if !running {
                t.say("  Compose exited non-zero — see the output above.");
            }
        }
    }

    // ---- what next
    t.say("");
    t.say("────────────────────────────────────────────────────────────");
    if running {
        t.say(&format!("  Control plane up.   {public_url}"));
    } else {
        t.say(&format!(
            "  Ready. Start it:    cd {} && docker compose up -d",
            dir.display()
        ));
    }
    t.say("");
    t.say("  Add a machine — open the UI, Nodes → add node, or run there:");
    t.say("");
    t.say(&format!(
        "    curl -fsSL {public_url}/install.sh | sh -s -- --token <join token>"
    ));
    t.say("");
    t.say("  Node connections arrive at:");
    t.say(&format!("    {agent_url}"));
    t.say(&format!("    fingerprint {fingerprint}"));
    if deployment == Deployment::ComposeTraefik {
        t.say("");
        t.say(&format!(
            "  Point {} at this host, NOT through a TLS-terminating proxy —",
            ServerAnswers::host_of(&agent_url)
        ));
        t.say("  the generated router passes the stream through untouched, and anything");
        t.say("  that opens it breaks node authentication.");
    }
    t.say("");
    t.say(&format!(
        "  Back up {} — SECRETS_KEY cannot be recovered.",
        dir.join(".env").display()
    ));
    t.say("────────────────────────────────────────────────────────────");
    Ok(())
}

/// Ask for a `DATABASE_URL` and check it before we build a deployment on it.
///
/// A typo here otherwise surfaces as a crash-looping container minutes later,
/// with the actual cause several layers down a log. Checking while the person
/// is still sitting at the prompt is the difference between a correction and an
/// investigation.
///
/// A failure is a warning, not a wall: running this on a box where Postgres is
/// not up yet — or is only reachable from inside a network this wizard is not
/// on — is perfectly legitimate, and refusing would block it for no good reason.
fn ask_database_url(t: &mut super::tty::Tty) -> Result<String> {
    loop {
        let url = t.text(
            "DATABASE_URL",
            Some("postgres://nook:password@localhost:5432/nook"),
        )?;

        match check_database(&url) {
            DbCheck::Ok(how) => {
                t.say(&format!("✓ Connected ({how})"));
                return Ok(url);
            }
            DbCheck::Failed(why) => {
                t.say(&format!("✗ Could not connect: {why}"));
                if t.confirm("Use it anyway?", false)? {
                    t.say("  Recorded. The control plane will retry on startup.");
                    return Ok(url);
                }
            }
            DbCheck::Unchecked => {
                t.say("  (no psql or docker here, so this was not verified)");
                return Ok(url);
            }
        }
    }
}

enum DbCheck {
    /// Verified, and how.
    Ok(&'static str),
    Failed(String),
    /// No tool available to check with. Saying so beats implying a pass.
    Unchecked,
}

/// Actually connect, rather than merely parsing the URL.
///
/// `psql` proves credentials, database name and permissions in one go. Falling
/// back to a container gets the same guarantee without installing a client. A
/// bare TCP probe is deliberately NOT used as a substitute: it would succeed
/// against the right host with the wrong password, which is a worse outcome
/// than admitting the check did not happen.
fn check_database(url: &str) -> DbCheck {
    let probe = |mut cmd: std::process::Command, how: &'static str| -> Option<DbCheck> {
        let out = cmd.output().ok()?;
        Some(if out.status.success() {
            DbCheck::Ok(how)
        } else {
            // The LAST line, not the first. psql tries ::1 before 127.0.0.1
            // and narrates each attempt, so the opening line is usually
            // "connection refused" for an address that was never the point —
            // which hides the actual cause (a wrong password, a missing
            // database) underneath plausible-looking noise.
            let err = String::from_utf8_lossy(&out.stderr);
            DbCheck::Failed(
                err.lines()
                    .map(str::trim)
                    .rfind(|l| !l.is_empty())
                    .unwrap_or("could not connect")
                    .to_string(),
            )
        })
    };

    if have("psql") {
        let mut c = std::process::Command::new("psql");
        c.args([url, "-tAc", "select 1"]);
        if let Some(r) = probe(c, "psql") {
            return r;
        }
    }
    if have("docker") {
        let mut c = std::process::Command::new("docker");
        // --network host so `localhost` in the URL means what the person meant.
        c.args([
            "run",
            "--rm",
            "--network",
            "host",
            "postgres:16-alpine",
            "psql",
            url,
            "-tAc",
            "select 1",
        ]);
        if let Some(r) = probe(c, "docker + psql") {
            return r;
        }
    }
    DbCheck::Unchecked
}

/// Generate the agent listener's certificate, returning its SHA-256.
///
/// Self-signed on purpose: nodes pin this fingerprint, which is a stronger
/// statement than "some public CA vouched for this name" — any of hundreds of
/// CAs could be persuaded to issue for it, and none of them can issue something
/// that matches a pin.
fn agent_certificate(dir: &Path, agent_url: &str) -> Result<String> {
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};

    let certs = dir.join("agent-certs");
    std::fs::create_dir_all(&certs)?;
    let crt = certs.join("agent.crt");
    let key = certs.join("agent.key");

    if crt.exists() {
        let pem = std::fs::read_to_string(&crt)?;
        return fingerprint_of_pem(&pem);
    }

    let host = ServerAnswers::host_of(agent_url);
    let kp = KeyPair::generate()?;
    let mut params = CertificateParams::new(vec![host.clone()])?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, host);
    params.distinguished_name = dn;
    let cert = params.self_signed(&kp)?;

    std::fs::write(&crt, cert.pem())?;
    write_private(&key, &kp.serialize_pem())?;
    fingerprint_of_pem(&cert.pem())
}

fn fingerprint_of_pem(pem: &str) -> Result<String> {
    use sha2::{Digest, Sha256};
    let der = rustls_pemfile::certs(&mut pem.as_bytes())
        .next()
        .context("no certificate in the generated PEM")??;
    Ok(format!("{:x}", Sha256::digest(&der)))
}

/// Write a file only the owner can read. Created with the right mode from the
/// start — a chmod afterwards leaves a window where the secret is world-readable.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("cannot write {}", path.display()))?;
        f.write_all(contents.as_bytes())?;
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_long_random_hex() {
        let a = secret();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, secret(), "two calls must not agree");
    }

    /// The generated certificate must fingerprint to the value we hand out; if
    /// these ever disagree every node refuses to connect, with a pin mismatch
    /// that looks like an attack rather than a bug.
    #[test]
    fn the_printed_fingerprint_matches_the_certificate() {
        let dir = std::env::temp_dir().join(format!("nook-init-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let fp = agent_certificate(&dir, "https://agent.example.com").unwrap();
        let pem = std::fs::read_to_string(dir.join("agent-certs/agent.crt")).unwrap();
        assert_eq!(fp, fingerprint_of_pem(&pem).unwrap());
        assert_eq!(fp.len(), 64);

        // Re-running must NOT mint a new certificate: that would invalidate the
        // pin every node was given.
        assert_eq!(
            fp,
            agent_certificate(&dir, "https://agent.example.com").unwrap()
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(dir.join("agent-certs/agent.key")).unwrap();
            assert_eq!(m.permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
