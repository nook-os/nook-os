//! Turning answers into deployment files.
//!
//! Kept as pure functions — answers in, file contents out — because these are
//! the parts of an installer whose breakage is silent. A compose file that
//! points at `latest` still starts. An `.env` that kept the example's
//! placeholder secret still boots, serves traffic, and signs sessions with a
//! key that is published on GitHub. Nothing goes wrong until it matters, so the
//! tests below assert on the generated text rather than on "did it run".

use std::fmt::Write as _;

/// How the control plane will be run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deployment {
    /// Compose, ports published directly.
    Compose,
    /// Compose behind an existing Traefik.
    ComposeTraefik,
    /// Print `docker run` lines for someone orchestrating elsewhere.
    DockerRun,
    /// The binary on the host, against a Postgres that already exists.
    Systemd,
}

/// Everything the generators need. Collected by the prompts, or by flags.
#[derive(Debug, Clone)]
pub struct ServerAnswers {
    /// e.g. `https://nook.example.com`
    pub public_url: String,
    /// Where nodes connect. Often a different name — see `agent_public_url`.
    pub agent_url: String,
    pub deployment: Deployment,
    /// Pinned, never `latest`.
    pub version: String,
    /// `None` means "bring your own", which is the only option without compose.
    pub postgres_password: Option<String>,
    pub database_url: Option<String>,
    pub session_secret: String,
    pub secrets_key: String,
    pub oidc: Option<Oidc>,
    /// Sign-in with no identity provider. Refused in production by the control
    /// plane itself; offered here only so someone can look around first.
    pub dev_auth: bool,
    pub tenant_name: String,
    /// The operator's own Giphy key, if they chose to bring one (MAIN-171
    /// AC-6). `None` is both the default and a fully working deployment — chat
    /// just has no GIF button — so nothing here nags about it being unset.
    pub giphy_key: Option<String>,
    /// The zone tunnels are served under, e.g. `tunnels.example.com`
    /// (MAIN-511). `None` is the norm and the default: a tunnel host needs a
    /// wildcard DNS record and a wildcard certificate, and this generator can
    /// create neither — so it is asked for, never inferred from `public_url`.
    pub tunnel_domain: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Oidc {
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
}

impl ServerAnswers {
    /// The hostname a Traefik router should match, derived from the URL.
    pub fn host_of(url: &str) -> String {
        url.trim_end_matches('/')
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("")
            .to_string()
    }

    pub fn api_host(&self) -> String {
        Self::host_of(&self.public_url)
    }

    pub fn agent_host(&self) -> String {
        Self::host_of(&self.agent_url)
    }

    /// The tunnel zone, normalised exactly the way the control plane
    /// normalises `TUNNEL_DOMAIN` (`nook_infra::config`): trimmed, undotted at
    /// both ends, lowercase. Same normalisation on both sides is what lets the
    /// router and the surface agree on which hosts are tunnels; a blank answer
    /// is `None`, not an empty zone, because an empty one would generate a rule
    /// matching every single-label host on the internet.
    pub fn tunnel_zone(&self) -> Option<String> {
        self.tunnel_domain
            .as_deref()
            .map(|d| d.trim().trim_matches('.').to_ascii_lowercase())
            .filter(|d| !d.is_empty())
    }
}

/// The regular expression matching exactly one label beneath `zone`.
///
/// `HostRegexp`, because Traefik has no wildcard `Host()`. One label, with no
/// dot in the character class, is what keeps the apex out (MAIN-511 AC-4):
/// `zone` itself has nothing in front of it and cannot match, so the generated
/// web and API routers keep answering for it. `[a-z0-9-]` is the whole
/// alphabet a tunnel label can be built from (`nook_proto::tunnel::slug_label`).
fn tunnel_host_regexp(zone: &str) -> String {
    format!(r"^[a-z0-9-]+\.{}$", zone.replace('.', r"\."))
}

/// A value as it must be WRITTEN into the compose file, because two readers get
/// to it before Traefik does.
///
/// YAML halves a backslash inside a double-quoted scalar, so a lone `\.` would
/// arrive as `.` — matching any character and widening the rule past the zone
/// it names. Compose then interpolates `$`: v2 leaves a stray one alone, but
/// v1 — which this wizard still offers to drive — refuses the whole file with
/// "Invalid interpolation format", so the end anchor is written as compose's
/// `$$` escape and reaches Traefik as one `$`.
fn compose_escape(value: &str) -> String {
    value.replace('\\', r"\\").replace('$', "$$")
}

/// The `.env` the control plane reads. Written 0600 by the caller.
pub fn env_file(a: &ServerAnswers) -> String {
    let mut s = String::new();
    s.push_str("# NookOS control plane — generated by `nook server init`.\n");
    s.push_str("# Secrets here are unique to this deployment. Back it up; do not commit it.\n\n");

    // `APP_ENV` and `AUTH_DEV_MODE` are not independent: the control plane
    // refuses to start when dev sign-in is enabled in production, and rightly
    // so. Emitting the combination anyway would ship an installer whose output
    // does not boot — which is exactly what happened the first time this ran.
    s.push_str(if a.dev_auth {
        "APP_ENV=dev\n"
    } else {
        "APP_ENV=production\n"
    });
    s.push_str("CONTROL_PLANE_BIND=0.0.0.0:8080\n");
    let _ = writeln!(s, "PUBLIC_BASE_URL={}", a.public_url);
    let _ = writeln!(s, "WEB_ORIGIN={}", a.public_url);
    s.push_str("RUST_LOG=nook_control=info,nook=info\n\n");

    let db = a.database_url.clone().unwrap_or_else(|| {
        format!(
            "postgres://nook:{}@postgres:5432/nook",
            a.postgres_password.as_deref().unwrap_or("nook")
        )
    });
    let _ = writeln!(s, "DATABASE_URL={db}");
    if let Some(pw) = &a.postgres_password {
        let _ = writeln!(s, "POSTGRES_PASSWORD={pw}");
    }
    s.push('\n');

    s.push_str("# Sessions are signed with this. Rotating it signs everyone out.\n");
    let _ = writeln!(s, "SESSION_SECRET={}", a.session_secret);
    s.push_str("SESSION_TTL_HOURS=168\n");
    s.push_str("# At-rest encryption for stored secrets. LOSING THIS LOSES THEM.\n");
    let _ = writeln!(s, "SECRETS_KEY={}\n", a.secrets_key);

    match &a.oidc {
        Some(o) => {
            let _ = writeln!(s, "OIDC_ISSUER_URL={}", o.issuer_url);
            let _ = writeln!(s, "OIDC_CLIENT_ID={}", o.client_id);
            let _ = writeln!(s, "OIDC_CLIENT_SECRET={}", o.client_secret);
            let _ = writeln!(
                s,
                "OIDC_REDIRECT_URL={}/api/v1/auth/callback",
                a.public_url.trim_end_matches('/')
            );
            s.push_str("OIDC_SCOPES=\"openid profile email\"\n");
        }
        None => {
            s.push_str("# No identity provider configured yet. Fill these in and restart.\n");
            s.push_str("OIDC_ISSUER_URL=\nOIDC_CLIENT_ID=\nOIDC_CLIENT_SECRET=\n");
            let _ = writeln!(
                s,
                "OIDC_REDIRECT_URL={}/api/v1/auth/callback",
                a.public_url.trim_end_matches('/')
            );
            s.push_str("OIDC_SCOPES=\"openid profile email\"\n");
        }
    }
    if a.dev_auth {
        s.push_str("\n# Sign in without an identity provider, for evaluation. APP_ENV is dev\n");
        s.push_str("# above BECAUSE of this: the control plane refuses to start with dev\n");
        s.push_str("# sign-in enabled in production. Configure OIDC, delete this line, and\n");
        s.push_str("# set APP_ENV=production before anyone else can reach it.\n");
        s.push_str("AUTH_DEV_MODE=true\n");
    }
    s.push('\n');
    let _ = writeln!(s, "DEFAULT_TENANT_NAME={}", a.tenant_name);

    // Giphy (MAIN-171 AC-6): emitted ONLY when the operator brought a key.
    // Writing an empty `NOOK_GIPHY_KEY=` would read as a half-configured
    // feature waiting to be finished, when in fact absent is a complete,
    // supported state — so the file says nothing at all instead (NG-3).
    if let Some(key) = &a.giphy_key {
        s.push_str("\n# Chat's GIF picker. Yours, from https://developers.giphy.com — it\n");
        s.push_str("# reaches signed-in browsers, so treat it as public, not as a secret.\n");
        let _ = writeln!(s, "NOOK_GIPHY_KEY={key}");
    }

    s.push_str("\n# The agent listener terminates TLS in-process (only it knows which\n");
    s.push_str("# tenant's CA to judge a client certificate against), so it does not sit\n");
    s.push_str("# behind the proxy that fronts the API.\n");
    s.push_str("NOOK_AGENT_BIND=0.0.0.0:8081\n");
    s.push_str("NOOK_AGENT_TLS_CERT=/etc/nook/agent.crt\n");
    s.push_str("NOOK_AGENT_TLS_KEY=/etc/nook/agent.key\n");
    let _ = writeln!(s, "NOOK_AGENT_PUBLIC_URL={}", a.agent_url);
    s
}

/// `docker-compose.yml`.
pub fn compose_file(a: &ServerAnswers) -> String {
    let traefik = a.deployment == Deployment::ComposeTraefik;
    // Traefik only (MAIN-511 NG-1): the other compose mode publishes ports and
    // has nothing routing by host, so a `TUNNEL_DOMAIN` there would advertise
    // a surface no request could ever arrive at.
    let tunnel_zone = if traefik { a.tunnel_zone() } else { None };
    let v = &a.version;
    let mut s = String::new();

    s.push_str("# NookOS control plane — generated by `nook server init`.\n");
    s.push_str("#\n# Image tags are pinned. `latest` would make an unrelated `docker compose\n");
    s.push_str("# up` a surprise upgrade, and a rollback an archaeology exercise.\n\n");

    if traefik {
        s.push_str("networks:\n  traefik_default:\n    external: true\n  internal:\n\n");
    }

    s.push_str("services:\n");

    // ---- postgres
    s.push_str("  postgres:\n    image: postgres:16-alpine\n    restart: unless-stopped\n");
    s.push_str("    environment:\n      POSTGRES_USER: nook\n");
    s.push_str("      POSTGRES_PASSWORD: ${POSTGRES_PASSWORD}\n      POSTGRES_DB: nook\n");
    s.push_str("    volumes:\n      - pgdata:/var/lib/postgresql/data\n");
    if traefik {
        s.push_str("    networks: [internal]\n");
    }
    s.push_str("    healthcheck:\n      test: [\"CMD-SHELL\", \"pg_isready -U nook\"]\n");
    s.push_str("      interval: 5s\n      timeout: 5s\n      retries: 30\n\n");

    // ---- control plane
    let _ = writeln!(
        s,
        "  control-plane:\n    image: ghcr.io/nook-os/nook-control:{v}"
    );
    s.push_str("    restart: unless-stopped\n    env_file: .env\n");
    if let Some(zone) = &tunnel_zone {
        s.push_str("    environment:\n");
        s.push_str("      # Beside the router below, so the two cannot name different zones:\n");
        s.push_str("      # the router decides which hosts arrive, this decides which the\n");
        s.push_str("      # control plane treats as tunnels.\n");
        let _ = writeln!(s, "      TUNNEL_DOMAIN: {zone}");
    }
    s.push_str(
        "    volumes:\n      # The agent listener's certificate. Nodes pin its fingerprint.\n",
    );
    s.push_str("      - ./agent-certs:/etc/nook:ro\n");
    s.push_str("    depends_on:\n      postgres:\n        condition: service_healthy\n");
    if traefik {
        s.push_str("    expose: [\"8080\"]\n");
        s.push_str("    networks:\n      internal:\n      traefik_default:\n");
        s.push_str("    labels:\n");
        s.push_str("      - \"traefik.enable=true\"\n");
        s.push_str("      - \"traefik.docker.network=traefik_default\"\n");
        let _ = writeln!(
            s,
            "      - \"traefik.http.routers.nook-api.rule=Host(`{}`) && (PathPrefix(`/api`) || PathPrefix(`/mcp`) || PathPrefix(`/healthz`) || PathPrefix(`/.well-known`) || Path(`/install.sh`))\"",
            a.api_host()
        );
        s.push_str("      - \"traefik.http.routers.nook-api.entrypoints=websecure\"\n");
        s.push_str("      - \"traefik.http.routers.nook-api.tls=true\"\n");
        s.push_str("      - \"traefik.http.routers.nook-api.priority=20\"\n");
        s.push_str("      - \"traefik.http.services.nook-api.loadbalancer.server.port=8080\"\n");
        s.push_str("      # Nodes, by SNI, in PASSTHROUGH mode. This is load-bearing: the\n");
        s.push_str("      # control plane must see the client certificate itself, so the proxy\n");
        s.push_str("      # may route this stream but must never open it. Terminating here\n");
        s.push_str("      # breaks mutual TLS and the fingerprint pin in one go.\n");
        let _ = writeln!(
            s,
            "      - \"traefik.tcp.routers.nook-agent.rule=HostSNI(`{}`)\"",
            a.agent_host()
        );
        s.push_str("      - \"traefik.tcp.routers.nook-agent.entrypoints=websecure\"\n");
        s.push_str("      - \"traefik.tcp.routers.nook-agent.tls.passthrough=true\"\n");
        s.push_str("      - \"traefik.tcp.services.nook-agent.loadbalancer.server.port=8081\"\n");
        if let Some(zone) = &tunnel_zone {
            s.push_str("      # Tunnels (MAIN-511). Every path of every host one label beneath\n");
            s.push_str("      # the zone, to the control plane — deliberately NOT to `web`,\n");
            s.push_str("      # whose SPA fallback would answer a tunnel host with the\n");
            s.push_str("      # application instead of NookOS's own \"no such tunnel\" page.\n");
            let _ = writeln!(
                s,
                "      - \"traefik.http.routers.nook-tunnels.rule=HostRegexp(`{}`)\"",
                compose_escape(&tunnel_host_regexp(zone))
            );
            s.push_str("      - \"traefik.http.routers.nook-tunnels.entrypoints=websecure\"\n");
            s.push_str("      # The apex router's service, reused rather than declared again:\n");
            s.push_str("      # it already points at 8080, and a second HTTP service on this\n");
            s.push_str("      # container would leave `nook-api`'s own router ambiguous.\n");
            s.push_str("      - \"traefik.http.routers.nook-tunnels.service=nook-api\"\n");
            s.push_str("      - \"traefik.http.routers.nook-tunnels.tls=true\"\n");
            s.push_str("      # A wildcard certificate has to be ASKED for. Given only a rule,\n");
            s.push_str("      # Traefik requests one per host — impossible for a name invented\n");
            s.push_str("      # when a tunnel opens — and serves TRAEFIK DEFAULT CERT instead.\n");
            s.push_str("      # A wildcard means DNS-01, so the entrypoint's resolver must be\n");
            s.push_str("      # a DNS-01 one; HTTP-01 cannot answer for `*`.\n");
            let _ = writeln!(
                s,
                "      - \"traefik.http.routers.nook-tunnels.tls.domains[0].main=*.{zone}\""
            );
            s.push_str("      # Below the apex routers (20 and 10). They cannot both match a\n");
            s.push_str("      # request, so this decides nothing today — it is here so that if\n");
            s.push_str("      # one ever does, precedence is a number somebody wrote rather\n");
            s.push_str("      # than Traefik's rule-length default.\n");
            s.push_str("      - \"traefik.http.routers.nook-tunnels.priority=5\"\n");
        }
    } else {
        s.push_str("    ports:\n      - \"8080:8080\"\n      - \"8081:8081\"\n");
    }
    s.push('\n');

    // ---- web
    let _ = writeln!(s, "  web:\n    image: ghcr.io/nook-os/nook-web:{v}");
    s.push_str("    restart: unless-stopped\n");
    s.push_str("    environment:\n      CONTROL_PLANE_ORIGIN: http://control-plane:8080\n");
    s.push_str("    depends_on: [control-plane]\n");
    if traefik {
        // 8080, not 80: the image's nginx is unprivileged and binds 8080
        // (MAIN-650). `expose` is the CONTAINER's port, so this is the number
        // Traefik connects to -- at 80 the router resolves and then 502s.
        s.push_str(
            "    expose: [\"8080\"]\n    networks:\n      internal:\n      traefik_default:\n",
        );
        s.push_str("    labels:\n      - \"traefik.enable=true\"\n");
        s.push_str("      - \"traefik.docker.network=traefik_default\"\n");
        let _ = writeln!(
            s,
            "      - \"traefik.http.routers.nook-web.rule=Host(`{}`)\"",
            a.api_host()
        );
        s.push_str("      - \"traefik.http.routers.nook-web.entrypoints=websecure\"\n");
        s.push_str("      - \"traefik.http.routers.nook-web.tls=true\"\n");
        s.push_str("      - \"traefik.http.routers.nook-web.priority=10\"\n");
        s.push_str("      - \"traefik.http.services.nook-web.loadbalancer.server.port=8080\"\n");
    } else {
        // Host 80 is unchanged -- people type no port in a browser. Only the
        // container side moves, because that is where nginx now listens.
        s.push_str("    ports:\n      - \"80:8080\"\n");
    }

    s.push_str("\nvolumes:\n  pgdata:\n");
    s
}

/// `docker run` lines, for someone whose orchestration lives elsewhere.
pub fn docker_run_script(a: &ServerAnswers) -> String {
    let mut s = String::new();
    s.push_str("# NookOS control plane, without compose.\n");
    s.push_str("# Bring your own Postgres; DATABASE_URL is in .env.\n\n");
    s.push_str("docker network create nook 2>/dev/null || true\n\n");
    let _ = writeln!(
        s,
        "docker run -d --name nook-control --network nook --restart unless-stopped \\\n  \
           --env-file .env \\\n  \
           -v \"$PWD/agent-certs:/etc/nook:ro\" \\\n  \
           -p 8080:8080 -p 8081:8081 \\\n  \
           ghcr.io/nook-os/nook-control:{}\n",
        a.version
    );
    let _ = writeln!(
        s,
        "docker run -d --name nook-web --network nook --restart unless-stopped \\\n  \
           -e CONTROL_PLANE_ORIGIN=http://nook-control:8080 \\\n  \
           -p 80:8080 \\\n  \
           ghcr.io/nook-os/nook-web:{}",
        a.version
    );
    s
}

/// The systemd unit for a node agent.
///
/// `user` selects `systemctl --user` (no `User=`, since the manager already
/// runs as that person) versus a system unit.
pub fn node_unit(user_mode: bool, exec: &str, home: &str, unix_user: &str) -> String {
    let mut s = String::new();
    s.push_str("# NookOS node agent — generated by `nook setup`.\n\n");
    s.push_str("[Unit]\nDescription=NookOS node agent\n");
    // network.target, NOT network-online.target (MAIN-363): the agent dials out
    // with jittered backoff from one second, so an early first attempt costs a
    // retry, while waiting for network-online costs minutes wherever nothing
    // owns the interface — on WSL that wait times out after two, and the node
    // is unreachable for all of it after every boot.
    s.push_str("After=network.target\n\n");
    s.push_str("[Service]\nType=simple\n");
    if !user_mode {
        let _ = writeln!(s, "User={unix_user}\nGroup={unix_user}");
    }
    let _ = writeln!(s, "ExecStart={exec} run");
    s.push_str("Restart=always\nRestartSec=5\n");
    s.push_str("Environment=RUST_LOG=nook=info\n");
    let _ = writeln!(s, "Environment=HOME={home}\nWorkingDirectory={home}");
    s.push_str("\n# THE important line. tmux is the buffer of record: sessions are meant to\n");
    s.push_str("# survive the agent restarting, and `Restart=always` means it will restart —\n");
    s.push_str("# on a network blip, a control-plane deploy, a crash. systemd's default\n");
    s.push_str("# KillMode kills the whole control group, which includes the tmux server the\n");
    s.push_str("# agent started (its own private `nook-<slug>` server — MAIN-108), so every\n");
    s.push_str("# one of those restarts would silently take the node's sessions with it.\n");
    s.push_str("# Stop the agent only; leave its tmux alone.\n");
    s.push_str("KillMode=process\n");
    if !user_mode {
        s.push_str(
            "\n# The node's whole job is spawning dev tooling — keep hardening light but real.\n",
        );
        s.push_str("NoNewPrivileges=yes\nProtectSystem=full\n");
        let _ = writeln!(s, "ReadWritePaths={home} /tmp");
    }
    s.push_str("\n[Install]\n");
    s.push_str(if user_mode {
        "WantedBy=default.target\n"
    } else {
        "WantedBy=multi-user.target\n"
    });
    s
}

/// A supervisord program config.
///
/// Two lines carry the whole reason self-update is safe under supervisord, and
/// both are easy to get wrong:
///
/// - `autorestart=true`, NOT `unexpected`. Self-update exits 0 on purpose so
///   the supervisor restarts it into the new binary. supervisord treats exit 0
///   as a clean, EXPECTED stop, so under `autorestart=unexpected` — a common
///   default in examples — the node would simply stay down after every update.
/// - `stopasgroup`/`killasgroup` = false. The agent starts its own tmux server
///   (a private `nook-<slug>` socket — MAIN-108) that holds the node's sessions,
///   and those are meant to outlive an agent restart. supervisord defaults these
///   to false already; setting them makes sure a future default change cannot
///   quietly take the sessions down. This is the same concern systemd's
///   `KillMode=process` addresses.
pub fn node_supervisord_conf(exec: &str, home: &str, user: &str) -> String {
    format!(
        "# NookOS node agent — generated by `nook setup`.\n\
         [program:nook-node]\n\
         command={exec} run\n\
         directory={home}\n\
         user={user}\n\
         autostart=true\n\
         autorestart=true\n\
         startsecs=3\n\
         stopsignal=TERM\n\
         stopasgroup=false\n\
         killasgroup=false\n\
         environment=HOME=\"{home}\",RUST_LOG=\"nook=info\"\n\
         stdout_logfile=/var/log/nook-node.log\n\
         stderr_logfile=/var/log/nook-node.err.log\n"
    )
}

/// A launchd agent for macOS.
///
/// The equivalent of the systemd user unit: runs as the person, starts at
/// login, and restarts on its own. `KeepAlive` is the counterpart of
/// `Restart=always` — without it launchd starts the agent once and never
/// again, which looks identical to working right up until the first network
/// blip.
///
/// Unlike systemd there is no KillMode to worry about: launchd does not kill
/// the process group by default, so the node's tmux server (its private
/// `nook-<slug>` socket — MAIN-108) survives a restart of the agent without any
/// special handling.
pub fn node_launchd_plist(exec: &str, home: &str, label: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exec}</string>
    <string>run</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>WorkingDirectory</key>
  <string>{home}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key>
    <string>{home}</string>
    <key>RUST_LOG</key>
    <string>nook=info</string>
    <!-- launchd gives an agent a minimal PATH. The node spawns the person's
         own tooling, so it needs the paths a login shell would have —
         Homebrew on both Apple silicon and Intel, plus the usual. -->
    <key>PATH</key>
    <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>StandardOutPath</key>
  <string>{home}/Library/Logs/nook-node.log</string>
  <key>StandardErrorPath</key>
  <string>{home}/Library/Logs/nook-node.log</string>
</dict>
</plist>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse the way dotenvy will: assignments only, comments ignored.
    ///
    /// Asserting on `contains` treats prose in a comment as configuration,
    /// which is both a false alarm and, worse, capable of hiding a real one.
    fn settings(env: &str) -> std::collections::HashMap<String, String> {
        env.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_string(), v.trim_matches('"').to_string()))
            .collect()
    }

    /// The unit must not wait for `network-online.target` (MAIN-363). Where
    /// nothing owns the interface — WSL is the case that bit us — the wait times
    /// out after two minutes and fails, and the node is unreachable for all of
    /// it, having done nothing. Backoff already covers an early first dial.
    #[test]
    fn the_node_unit_does_not_wait_for_network_online() {
        for user_mode in [true, false] {
            let unit = node_unit(user_mode, "/usr/bin/nook", "/home/u", "u");
            let directives: Vec<&str> = unit
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .collect();
            assert!(
                directives.contains(&"After=network.target"),
                "expected After=network.target, got {directives:?}"
            );
            assert!(
                !directives.iter().any(|l| l.contains("network-online")),
                "unit still orders on network-online: {directives:?}"
            );
        }
    }

    fn answers(deployment: Deployment) -> ServerAnswers {
        ServerAnswers {
            public_url: "https://nook.example.com".into(),
            agent_url: "https://agent.nook.example.com".into(),
            deployment,
            version: "v9.9.9".into(),
            postgres_password: Some("pg-secret".into()),
            database_url: None,
            session_secret: "s".repeat(64),
            secrets_key: "k".repeat(64),
            oidc: None,
            dev_auth: false,
            tenant_name: "acme".into(),
            giphy_key: None,
            tunnel_domain: None,
        }
    }

    /// The generated file, byte for byte, as it read before tunnels existed
    /// (MAIN-511 AC-6). Produced by the generator at the commit this branch
    /// started from, so "unchanged" is a comparison rather than a claim.
    const COMPOSE_TRAEFIK_BEFORE_TUNNELS: &str =
        include_str!("testdata/compose-traefik.golden.yml");

    /// Every install path this file writes must reach the web container on
    /// **8080**, because the image's nginx is unprivileged and binds that
    /// (MAIN-650). The Helm chart has indirection that made this correct --
    /// a Service `targetPort: http` resolved by name -- and this generator has
    /// none, so the number is written out three times and every one of them is
    /// load-bearing.
    ///
    /// Asserted across all three paths together, and asserted NEGATIVELY too:
    /// the failure this prevents is silent. Behind Traefik a stale `80` still
    /// routes and then 502s on connect; on the published-port paths the host
    /// binding maps to a dead container port and the SPA is simply unreachable.
    /// Nothing errors at generation time in either case.
    #[test]
    fn every_install_path_reaches_the_web_container_on_8080() {
        // Behind Traefik: `expose` and the service port are both the CONTAINER
        // side, so both move.
        let c = compose_file(&answers(Deployment::ComposeTraefik));
        assert!(
            c.contains("traefik.http.services.nook-web.loadbalancer.server.port=8080"),
            "{c}"
        );
        assert!(
            !c.contains("nook-web.loadbalancer.server.port=80\""),
            "a stale 80 here routes and then 502s: {c}"
        );

        // Published directly: the HOST side stays 80 -- nobody types a port in
        // a browser -- and only the container side moves.
        let c = compose_file(&answers(Deployment::Compose));
        assert!(c.contains("\"80:8080\""), "{c}");
        assert!(!c.contains("\"80:80\""), "{c}");

        // And the same for the path that has no compose file at all.
        let d = docker_run_script(&answers(Deployment::DockerRun));
        assert!(d.contains("-p 80:8080"), "{d}");
        assert!(!d.contains("-p 80:80 "), "{d}");
    }

    /// No tunnel domain has to mean no diff at all — not a router with an empty
    /// host, not `TUNNEL_DOMAIN=`, which the control plane would read as a zone
    /// and which Traefik would parse as a rule matching nothing. Most
    /// deployments will never set this, and none of them should be able to tell
    /// the question was added.
    #[test]
    fn without_a_tunnel_domain_the_compose_file_is_unchanged() {
        let mut a = answers(Deployment::ComposeTraefik);
        assert_eq!(compose_file(&a), COMPOSE_TRAEFIK_BEFORE_TUNNELS);

        // Blank and whitespace are how the prompt's "leave it empty" arrives.
        for blank in ["", "   ", ".", " . "] {
            a.tunnel_domain = Some(blank.into());
            assert_eq!(
                compose_file(&a),
                COMPOSE_TRAEFIK_BEFORE_TUNNELS,
                "{blank:?} is no zone"
            );
        }

        let c = compose_file(&answers(Deployment::ComposeTraefik));
        assert!(!c.contains("nook-tunnels"));
        assert!(!c.contains("TUNNEL_DOMAIN"));
    }

    /// The router, the wildcard certificate and the surface's own zone, all
    /// from the one answer (MAIN-511 AC-2/AC-3/AC-5).
    #[test]
    fn a_tunnel_domain_routes_the_wildcard_to_the_control_plane() {
        let mut a = answers(Deployment::ComposeTraefik);
        a.tunnel_domain = Some("Tunnels.Example.COM.".into());
        let c = compose_file(&a);

        assert!(
            c.contains(
                "traefik.http.routers.nook-tunnels.rule=HostRegexp(`^[a-z0-9-]+\\\\.tunnels\\\\.example\\\\.com$$`)"
            ),
            "{c}"
        );
        // 8080, by reusing the API router's service — the web service's SPA
        // fallback would answer a tunnel host with the application.
        assert!(c.contains("traefik.http.routers.nook-tunnels.service=nook-api"));
        assert!(c.contains("traefik.http.services.nook-api.loadbalancer.server.port=8080"));
        assert!(!c.contains("traefik.http.routers.nook-tunnels.service=nook-web"));

        // A wildcard certificate is requested, not hoped for.
        assert!(c.contains(
            "traefik.http.routers.nook-tunnels.tls.domains[0].main=*.tunnels.example.com"
        ));
        assert!(c.contains("traefik.http.routers.nook-tunnels.tls=true"));

        // And the surface is switched on with the same zone the router matches.
        assert!(
            c.contains("\n      TUNNEL_DOMAIN: tunnels.example.com\n"),
            "{c}"
        );

        // Below the apex routers, so precedence is stated rather than inherited.
        assert!(c.contains("traefik.http.routers.nook-tunnels.priority=5"));
        assert!(c.contains("traefik.http.routers.nook-api.priority=20"));
        assert!(c.contains("traefik.http.routers.nook-web.priority=10"));
    }

    /// AC-4, checked with a regex engine rather than by reading the pattern
    /// twice. Traefik v3 compiles `HostRegexp` with Go's RE2, which agrees with
    /// this crate on every construct the rule uses.
    ///
    /// The apex is the case that matters: if the tunnel rule matched it, the
    /// whole application would be answered by the tunnel surface's "no such
    /// tunnel" page.
    #[test]
    fn the_tunnel_rule_never_matches_the_apex() {
        let re = regex::Regex::new(&tunnel_host_regexp("tunnels.example.com")).unwrap();

        for label in ["web-main-3000", "api", "x", "a-2", &"n".repeat(63)] {
            assert!(
                re.is_match(&format!("{label}.tunnels.example.com")),
                "{label} is a label NookOS can mint"
            );
        }

        for host in [
            "tunnels.example.com",              // the zone itself
            "nook.example.com",                 // the apex
            "example.com",                      // the parent
            "web.tunnels.example.net",          // a different TLD
            "web.tunnelsxexample.com",          // the dots are literal, not `.`
            "evil.com/x.tunnels.example.com",   // not anchored at the front
            "web.tunnels.example.com.evil.com", // nor at the back
        ] {
            assert!(!re.is_match(host), "{host} must not be a tunnel host");
        }
    }

    /// The rule has to survive YAML and compose interpolation before it is a
    /// regular expression: a halved backslash leaves `.` matching any
    /// character, and a bare `$` is what Compose v1 rejects the whole file for.
    #[test]
    fn the_rule_is_escaped_for_the_file_it_is_written_into() {
        assert_eq!(
            compose_escape(&tunnel_host_regexp("tunnels.example.com")),
            r"^[a-z0-9-]+\\.tunnels\\.example\\.com$$"
        );
    }

    /// NG-1: only the mode with a proxy in front of it. Emitting `TUNNEL_DOMAIN`
    /// where nothing routes by host would advertise a URL that resolves to a
    /// deployment unable to answer it.
    #[test]
    fn tunnels_reach_only_the_traefik_mode() {
        let mut a = answers(Deployment::Compose);
        a.tunnel_domain = Some("tunnels.example.com".into());
        let c = compose_file(&a);
        assert!(!c.contains("TUNNEL_DOMAIN"), "{c}");
        assert!(!c.contains("nook-tunnels"), "{c}");
    }

    /// MAIN-171 AC-6/NG-3: Giphy is offered, never required. Skipping it must
    /// leave the `.env` with no trace of it — not an empty `NOOK_GIPHY_KEY=`,
    /// which reads as a half-finished setup rather than the complete, supported
    /// state it actually is.
    #[test]
    fn giphy_is_opt_in_and_absent_when_skipped() {
        let cfg = settings(&env_file(&answers(Deployment::Compose)));
        assert_eq!(cfg.get("NOOK_GIPHY_KEY"), None);

        let mut a = answers(Deployment::Compose);
        a.giphy_key = Some("gk-live-abc".into());
        assert_eq!(
            settings(&env_file(&a))["NOOK_GIPHY_KEY"],
            "gk-live-abc",
            "a key the operator brought must reach the control plane"
        );
    }

    /// The placeholder in `.env.example` is sixty-four zeroes. Shipping that
    /// into a real deployment means every session cookie is forgeable by
    /// anyone who has read the repo — and nothing visibly breaks.
    #[test]
    fn secrets_are_generated_not_placeholders() {
        let cfg = settings(&env_file(&answers(Deployment::Compose)));
        assert_eq!(cfg["SESSION_SECRET"], "s".repeat(64));
        assert_eq!(cfg["SECRETS_KEY"], "k".repeat(64));
        assert_eq!(cfg["POSTGRES_PASSWORD"], "pg-secret");
        assert_ne!(
            cfg["SESSION_SECRET"],
            "0".repeat(64),
            "the .env.example placeholder"
        );
        // And the database URL must actually use that password, not the
        // default from the dev compose file.
        assert_eq!(
            cfg["DATABASE_URL"],
            "postgres://nook:pg-secret@postgres:5432/nook"
        );
    }

    /// A `latest` tag turns an unrelated `docker compose up` into a surprise
    /// upgrade, and there is no way to tell from the file what is running.
    #[test]
    fn images_are_pinned_never_latest() {
        for d in [Deployment::Compose, Deployment::ComposeTraefik] {
            let c = compose_file(&answers(d));
            assert!(c.contains("nook-control:v9.9.9"), "{d:?}");
            assert!(c.contains("nook-web:v9.9.9"), "{d:?}");
            assert!(!c.contains(":latest"), "{d:?} pinned to latest");
        }
        assert!(!docker_run_script(&answers(Deployment::DockerRun)).contains(":latest"));
    }

    /// The one that cost a live debugging session: a Traefik router that
    /// terminates TLS holds the client certificate and hands the control plane
    /// plaintext, so mutual auth silently stops being mutual.
    #[test]
    fn the_traefik_agent_router_passes_through() {
        let c = compose_file(&answers(Deployment::ComposeTraefik));
        assert!(c.contains("traefik.tcp.routers.nook-agent.tls.passthrough=true"));
        assert!(c.contains("HostSNI(`agent.nook.example.com`)"));
        assert!(c.contains("traefik.tcp.services.nook-agent.loadbalancer.server.port=8081"));
        // And it must NOT have been given a certificate resolver, which would
        // make Traefik terminate it after all.
        assert!(!c.contains("nook-agent.tls.certresolver"));
    }

    /// Without a proxy the ports have to be published, or nothing can reach
    /// either listener.
    #[test]
    fn plain_compose_publishes_both_ports() {
        let c = compose_file(&answers(Deployment::Compose));
        assert!(c.contains("\"8080:8080\""));
        assert!(c.contains("\"8081:8081\""));
        assert!(!c.contains("traefik.enable"));
    }

    #[test]
    fn hosts_are_derived_from_urls() {
        let a = answers(Deployment::ComposeTraefik);
        assert_eq!(a.api_host(), "nook.example.com");
        assert_eq!(a.agent_host(), "agent.nook.example.com");
        let mut b = a.clone();
        b.public_url = "http://box.local:8080/".into();
        assert_eq!(b.api_host(), "box.local", "port and path must be stripped");
    }

    #[test]
    fn oidc_redirect_is_built_from_the_public_url() {
        let mut a = answers(Deployment::Compose);
        a.public_url = "https://nook.example.com/".into();
        a.oidc = Some(Oidc {
            issuer_url: "https://auth.example.com".into(),
            client_id: "cid".into(),
            client_secret: "csec".into(),
        });
        // Exactly one slash — a doubled one is a redirect_uri mismatch at the
        // identity provider, which reads as "login is broken".
        let cfg = settings(&env_file(&a));
        assert_eq!(
            cfg["OIDC_REDIRECT_URL"],
            "https://nook.example.com/api/v1/auth/callback"
        );
    }

    /// launchd starts a job once unless told otherwise. Without KeepAlive the
    /// agent runs until the first disconnect and then quietly stays dead.
    #[test]
    fn the_launchd_agent_restarts_itself() {
        let p = node_launchd_plist("/Users/x/.local/bin/nook", "/Users/x", "dev.nookos.node");
        assert!(p.contains("<key>KeepAlive</key>"));
        assert!(p.contains("<key>RunAtLoad</key>"));
        // The node spawns the user's tooling; a minimal PATH breaks every
        // runtime that lives in Homebrew.
        assert!(
            p.contains("/opt/homebrew/bin"),
            "Apple silicon Homebrew missing"
        );
        assert!(p.contains("/usr/local/bin"), "Intel Homebrew missing");
    }

    /// Losing this is losing every terminal the user had open.
    #[test]
    fn both_unit_variants_keep_tmux_alive() {
        for user_mode in [true, false] {
            let u = node_unit(user_mode, "/home/x/.local/bin/nook", "/home/x", "x");
            assert!(u.contains("KillMode=process"), "user_mode={user_mode}");
        }
    }

    /// The two lines that make self-update safe under supervisord, asserted so
    /// they cannot be edited away. `autorestart=true` (a bare `exit 0` from a
    /// self-update must trigger a restart, which `unexpected` would not), and
    /// the group-kill flags off (so restarting the agent never reaps the tmux
    /// server holding the user's sessions).
    #[test]
    fn supervisord_conf_restarts_and_spares_tmux() {
        let c = node_supervisord_conf("/home/x/.local/bin/nook", "/home/x", "x");
        assert!(c.contains("autorestart=true"), "{c}");
        assert!(
            !c.contains("autorestart=unexpected"),
            "exit 0 from self-update must restart, which `unexpected` refuses"
        );
        assert!(c.contains("stopasgroup=false"), "{c}");
        assert!(c.contains("killasgroup=false"), "{c}");
        assert!(c.contains("command=/home/x/.local/bin/nook run"), "{c}");
        assert!(c.contains("[program:nook-node]"), "{c}");
    }

    /// A user unit that declares `User=` fails to start: the user manager is
    /// already that person and cannot switch.
    #[test]
    fn a_user_unit_does_not_set_user() {
        let u = node_unit(true, "/home/x/.local/bin/nook", "/home/x", "x");
        assert!(!u.contains("User="));
        assert!(u.contains("WantedBy=default.target"));

        let s = node_unit(false, "/home/x/.local/bin/nook", "/home/x", "x");
        assert!(s.contains("User=x"));
        assert!(s.contains("WantedBy=multi-user.target"));
    }

    /// `AUTH_DEV_MODE` only appears when asked for. The control plane refuses
    /// to boot with it under production, so emitting it by default would ship
    /// an installer whose output does not start.
    #[test]
    fn dev_auth_is_opt_in() {
        assert_eq!(
            settings(&env_file(&answers(Deployment::Compose))).get("AUTH_DEV_MODE"),
            None
        );
        let mut a = answers(Deployment::Compose);
        a.dev_auth = true;
        assert_eq!(settings(&env_file(&a))["AUTH_DEV_MODE"], "true");
    }

    /// The generated deployment has to actually start.
    ///
    /// `config.rs` bails with "AUTH_DEV_MODE must not be enabled when
    /// APP_ENV=production", so emitting both is an installer that writes a
    /// stack which crash-loops. The first real run of this wizard did exactly
    /// that; the assertion the earlier test was missing is this one.
    #[test]
    fn dev_auth_and_production_are_never_emitted_together() {
        let mut a = answers(Deployment::Compose);
        a.dev_auth = true;
        let cfg = settings(&env_file(&a));
        assert_eq!(cfg.get("APP_ENV").map(String::as_str), Some("dev"));
        assert_eq!(cfg.get("AUTH_DEV_MODE").map(String::as_str), Some("true"));

        a.dev_auth = false;
        let cfg = settings(&env_file(&a));
        assert_eq!(cfg.get("APP_ENV").map(String::as_str), Some("production"));
        assert_eq!(cfg.get("AUTH_DEV_MODE"), None);
    }

    /// Nodes are told where the agent listener is, which is not always where
    /// the API is.
    #[test]
    fn the_agent_url_is_recorded_for_joining_nodes() {
        let cfg = settings(&env_file(&answers(Deployment::Compose)));
        assert_eq!(
            cfg["NOOK_AGENT_PUBLIC_URL"],
            "https://agent.nook.example.com"
        );
    }
}

#[cfg(test)]
mod plist_dump {
    /// Writes the plist to `$NOOK_PLIST_OUT` so a real plist parser can check
    /// it — no Rust dependency here understands the format, and a malformed
    /// plist fails silently on macOS rather than loudly.
    ///
    ///   NOOK_PLIST_OUT=/tmp/n.plist cargo test -p nook-node dump_launchd_plist
    ///   python3 -c "import plistlib;print(plistlib.load(open('/tmp/n.plist','rb')))"
    ///
    /// Skipped unless the variable is set, so a normal run pays nothing.
    #[test]
    fn dump_launchd_plist() {
        if let Ok(path) = std::env::var("NOOK_PLIST_OUT") {
            let p = super::node_launchd_plist(
                "/Users/ryan/.local/bin/nook",
                "/Users/ryan",
                "dev.nookos.node",
            );
            std::fs::write(path, p).expect("write plist");
        }
    }
}
