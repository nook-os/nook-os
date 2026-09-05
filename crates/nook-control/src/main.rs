use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nook_db::dialect::type_mapping;
use nook_db::{params, Db};
use tracing_subscriber::EnvFilter;

use nook_control::{routes, AppState, Config, OidcSetup, MIGRATOR, SQUASH_MANIFEST};

#[derive(Parser)]
#[command(name = "nook-control", about = "NookOS control plane")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the control plane server (default).
    Serve,
    /// Seed the database with dev fixtures (idempotent).
    Seed,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    // After the subscriber, so the hook's chained default and the layer's
    // structured record both land in the configured log (MAIN-273).
    nook_errors::install_panic_hook();

    // A desktop install's shell asks for this (MAIN-400 AC-2); nothing else
    // sets the variable, so a server deployment is untouched. It is what stops
    // a force-quit of the app leaving this process holding the SQLite
    // single-instance lock, which would refuse the next launch.
    nook_desktop_env::exit_when_orphaned();

    let cli = Cli::parse();
    // Before anything reads config, so an operator whose deployment still sets a
    // retired variable hears about it in the same breath as the boot (MAIN-602).
    nook_infra::config::warn_retired_env();
    let cfg = Config::from_env()?;
    // Every boot, not just the first: an install whose values file said `dev`
    // months ago is still answering dev-login today, and the values file is not
    // where anyone looks (MAIN-671).
    nook_infra::config::warn_dev_login_open(&cfg);

    // One control plane per SQLite file (MAIN-197). Taken BEFORE the pool,
    // because `create_if_missing` means connecting is already a write, and the
    // value of this check is entirely in refusing before the first one.
    //
    // The binding is named on purpose. The lock lives on an open descriptor, so
    // `let _ = …` would drop it here and enforce nothing while looking correct;
    // holding it in `main`'s scope is what makes it last as long as the process.
    // Postgres takes no lock and multi-instance is unaffected.
    let _instance_lock = nook_db::acquire_single_instance_lock(&cfg.database_url)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("this control plane cannot use that database")?;
    if let Some(lock) = &_instance_lock {
        tracing::info!(lock = %lock.path().display(), "holding the SQLite single-instance lock");
    }

    // Select the engine from the DATABASE_URL scheme and refuse an unknown one
    // here, at boot, with a pointed message (MAIN-195). Postgres connects exactly
    // as before; the pool type is unchanged.
    let db = nook_db::connect(&cfg.database_url, 10)
        .await
        .context("opening the database")?;
    // The migration set follows the engine (MAIN-196): the Postgres track, or
    // the SQLite one for a `sqlite://` URL. On Postgres a pre-squash ledger
    // collapses to the canonical row first (MAIN-235) — the image that carries
    // a squash carries its own re-stamp, never the two-step ordering that
    // caused the documented prod near-miss — and dev tolerates a ledger ahead
    // of this checkout while production stays strictly fatal (MAIN-224).
    nook_db::migrate::run_boot_migrations_for(
        &db,
        cfg.is_production(),
        &MIGRATOR,
        &nook_control::MIGRATOR_SQLITE,
        SQUASH_MANIFEST,
    )
    .await?;

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            // Always seed: built-in themes ship with every install, so a fresh
            // production instance has real choice out of the box. `seed::run`
            // stops there in production — demo tenants and dev join tokens
            // stay dev-only.
            nook_control::seed::run(&db, &cfg).await?;
            serve(db, cfg).await
        }
        Command::Seed => nook_control::seed::run(&db, &cfg).await,
    }
}

async fn serve(db: nook_db::DbPool, cfg: Config) -> Result<()> {
    // Pick the TLS backend explicitly. Several crates in the tree pull rustls
    // with different providers (the AWS SDK among them), which leaves the
    // process-wide default ambiguous — and rustls panics rather than guessing.
    // Installing it here makes the choice ours and the failure impossible.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Discover the IdP once at startup. Failure is non-fatal so the stack
    // boots without the IdP reachable (dev-login still works).
    let oidc = match cfg.oidc_setup() {
        OidcSetup::Configured { issuer } => {
            match nook_control::auth::OidcContext::discover(issuer, &cfg, &db).await {
                Ok(ctx) => {
                    tracing::info!(issuer, "OIDC discovery complete");
                    Some(ctx)
                }
                Err(e) => {
                    tracing::warn!(issuer, error = %e, "OIDC discovery failed — IdP login disabled");
                    None
                }
            }
        }
        // An issuer with the set incomplete is an operator part-way through
        // configuring a provider (MAIN-527 AC-1). Reporting it as the local
        // case below would tell them the opposite of what they intended, at the
        // level that says nothing is wrong.
        OidcSetup::Partial { issuer, missing } => {
            tracing::warn!(
                issuer,
                missing = %missing.join(", "),
                "identity provider is partially configured — IdP login disabled until the missing settings are set"
            );
            None
        }
        OidcSetup::Absent => {
            // INFO, not WARN (MAIN-397 AC-3): on a local install there is no
            // identity provider by construction, so every launch would log a
            // warning about the expected state. A misconfigured IdP still warns
            // — that is the arm above, and the one where discovery failed.
            tracing::info!("no identity provider configured — sign-in is local accounts");
            None
        }
    };

    let bind = cfg.bind.clone();
    let agent_bind = cfg.agent_bind.clone();
    let agent_tls_cert = cfg.agent_tls_cert.clone();
    let agent_tls_key = cfg.agent_tls_key.clone();
    let is_production = cfg.is_production();
    let grace = std::time::Duration::from_secs(cfg.shutdown_grace_secs);
    let state = AppState::new(db, cfg, oidc).await;

    // If OIDC is configured but the boot-time discovery failed (the IdP was
    // unreachable), keep retrying in the background with backoff so login
    // recovers on its own — no container restart (MAIN-169 AC-1). A successful
    // login also triggers an immediate on-demand attempt; this task is the
    // unattended path. We do not re-discover after success (NG-4).
    if state.oidc.degraded() {
        let oidc = state.oidc.clone();
        tokio::spawn(async move {
            let mut backoff = std::time::Duration::from_secs(2);
            let max = std::time::Duration::from_secs(30);
            while oidc.degraded() {
                tokio::time::sleep(backoff).await;
                match oidc.discover_now().await {
                    // Success logs INFO with the issuer inside `discover_now`.
                    Ok(_) => break,
                    Err(e) => {
                        tracing::debug!(error = %e, "OIDC discovery retry failed — will retry");
                        backoff = (backoff * 2).min(max);
                    }
                }
            }
        });
    }

    // Join the cross-instance bus (LISTEN/NOTIFY): makes N control-plane
    // replicas cooperate. On a single instance it's a no-op fast path.
    // Cross-instance fan-out is Postgres LISTEN/NOTIFY. A SQLite deployment is
    // one process against one file — there is no second replica to coordinate
    // with — so the bus is simply not started, which the registry already
    // supports as its documented fast path ("without start_bus … identical to
    // the original in-memory registry"). Starting a Postgres listener on a
    // SQLite pool would panic, and faking a bus would be pretending to solve a
    // problem that cannot arise (MAIN-196).
    if state.db.engine() == nook_db::Engine::Postgres {
        state.registry.start_bus(state.db.clone());
    } else {
        tracing::info!(
            "single-instance engine — cross-instance event bus not started (in-memory fan-out only)"
        );
    }
    let instance = state.registry.instance_id();
    tracing::info!(%instance, "control plane instance");

    // Drain queued loop jobs onto eligible executors (MAIN-160). Every replica
    // runs it; the queue's atomic receive keeps them from double-claiming.
    nook_control::services::job_dispatch::start(state.clone());

    // Reap jobs whose executor node went dark, so a crashed/upgraded operator
    // never strands work (MAIN-164). Every replica runs it; the reap's atomic
    // conditional UPDATE keeps them from double-failing a job.
    nook_control::services::job_reaper::start(state.clone());

    // The same safety at the board-card layer (MAIN-229): requeue a card whose
    // claim holder is provably gone, escalate one held past the cap. Fenced to
    // leased cards, so a card a human put In Progress is never touched.
    nook_control::services::claim_reaper::start(state.clone());

    // Move a card to Done once its pull request has merged (MAIN-491), so the
    // board stops lying about shipped work between agent-skill runs. Gated on
    // `loops.enabled`; read-only against the forge, and every board write is a
    // guarded one, so every replica may run it.
    nook_control::services::merge_reconcile::start(state.clone());

    // Reclaim checkouts that have stayed tombstoned past the retention window
    // (MAIN-220): reconcile now marks a vanished checkout missing instead of
    // deleting it, so this is the only background path that hard-deletes the row
    // — and it logs any task still pointing at the reclaimed path.
    nook_control::services::workspace_reaper::start(state.clone());

    // Terminated sessions accumulate from ordinary human use as much as from
    // loops, so this one is NOT behind `loops.enabled` — an operator who turned
    // loops off would otherwise collect rows forever with no way to stop it.
    nook_control::services::session_reaper::start(state.clone());

    // Tear down tunnels nobody has used inside the window (MAIN-404 AC-3), so a
    // port exposed to a tenant is not held open until the next restart. Returns
    // immediately when no `TUNNEL_DOMAIN` is configured — there is nothing that
    // could have created one.
    nook_control::services::tunnel_reaper::start(state.clone());

    // Fire build runs for the workspaces whose owners enabled the build loop
    // (MAIN-385). Off for every workspace until a person says otherwise, and
    // gated on `loops.enabled` besides — with either off this is one indexed
    // lookup a tick and nothing else.
    nook_control::services::build_loop::start(state.clone());

    // Poll the mailboxes tenants configured, feeding the same inbound-email
    // pipeline the provider webhook does (MAIN-333). Not gated on
    // `loops.enabled`: receiving support mail is not loop work, and a tenant
    // that turned loops off still wants its reports filed. With no poller
    // configured this is one small scan every fifteen seconds and nothing else —
    // `email_pollers` holds at most one row per tenant, so it carries no index
    // beyond its primary key.
    nook_control::services::email_imap::start(state.clone());

    // Converge sessions to what workspaces declare (MAIN-316). Every replica
    // runs it; a partial unique index on live managed sessions per
    // (workspace, node) is what makes one starter win rather than a lease.
    // Gated on `sessions.reconcile.enabled`, default off.
    nook_control::services::session_reconcile::start(state.clone());

    // One signal, every listener. A single task watches for SIGTERM/SIGINT and
    // flips a watch channel; the browser door, the agent door, and the grace
    // timer each hold a receiver, so a rolling update drains all of them at once
    // rather than only whichever `axum::serve` happened to own the signal.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    let shutdown_db = state.db.clone();
    let router = routes::build_router(state.clone());

    // BOTH doors are bound before EITHER is served, and the browser door goes
    // first (MAIN-285).
    //
    // The agent door used to be bound *and served* before the browser port was
    // even attempted. A boot that was going to fail on the browser port
    // therefore still opened the agent door for the ~1s it took to get there —
    // long enough for every node to complete a WebSocket handshake and be cut
    // off when the process exited. Under a restart loop that is not a blip: it
    // is a permanent connect/close flap in which no node ever holds a session
    // long enough to finish a capability push, so the control plane serves
    // stale capabilities (a runtime reported `not_authorized` long after it was
    // authorized) and the node is undispatchable.
    //
    // Binding both first makes a doomed boot fail with the fleet's door still
    // shut: nodes see a refused connection, back off, and reconnect once a boot
    // actually succeeds. Binding the browser port first is what guarantees it
    // for the case that actually happens — the browser port is the contended
    // one, since it is the port everything else in a deployment also talks to.
    let (listener, agent_listener) = bind_doors(&bind, &agent_bind).await?;

    // The inbound SMTP receiver, when this deployment runs one (MAIN-334).
    // Bound here, with the other doors, and for the same reason: a boot that is
    // going to fail on a misconfigured mail port must fail before any node or
    // browser is served. `None` — the shipped default — opens no socket at all.
    let smtp_state = state.clone();
    let smtp = nook_control::services::email_smtp::bind(&smtp_state.cfg).await?;
    let agent_router = routes::build_agent_router(state);
    match (
        agent_tls_cert.as_deref().filter(|s| !s.is_empty()),
        agent_tls_key.as_deref().filter(|s| !s.is_empty()),
    ) {
        (Some(cert), Some(key)) => {
            // TLS terminates HERE, not at the proxy: only the control plane can
            // judge a client certificate against the right tenant's CA.
            let tls = nook_control::agent_tls::acceptor(cert, key)?;
            tracing::info!(bind = %agent_bind, "agent listener (mTLS)");
            let agent_shutdown = wait_for_shutdown(shutdown_rx.clone());
            tokio::spawn(nook_control::agent_tls::serve(
                agent_listener,
                agent_router,
                tls,
                agent_shutdown,
            ));
        }
        (None, None) if is_production => {
            // The agent port carries enrolment and every node's connection.
            // Serving it in the clear in production would put join tokens and
            // CSRs on the wire, and the warning below is too easy to miss in a
            // log — this is a misconfiguration that should stop the process.
            anyhow::bail!(
                "the agent listener on {agent_bind} would be PLAINTEXT: set \
                 NOOK_AGENT_TLS_CERT and NOOK_AGENT_TLS_KEY (see \
                 deploy/enable-agent-mtls.sh)"
            );
        }
        (None, None) => {
            tracing::warn!(
                bind = %agent_bind,
                "agent listener is PLAINTEXT — set NOOK_AGENT_TLS_CERT and \
                 NOOK_AGENT_TLS_KEY so node connections are mutually authenticated"
            );
            let agent_shutdown = wait_for_shutdown(shutdown_rx.clone());
            tokio::spawn(async move {
                if let Err(e) = axum::serve(agent_listener, agent_router)
                    .with_graceful_shutdown(agent_shutdown)
                    .await
                {
                    tracing::error!(error = %e, "agent listener stopped");
                }
            });
        }
        // Half-configured is a mistake worth failing on rather than quietly
        // serving plaintext to a fleet the operator believes is encrypted.
        _ => anyhow::bail!("NOOK_AGENT_TLS_CERT and NOOK_AGENT_TLS_KEY must be set together"),
    }

    if let Some(smtp) = smtp {
        tokio::spawn(nook_control::services::email_smtp::serve(
            smtp,
            smtp_state,
            wait_for_shutdown(shutdown_rx.clone()),
        ));
    }

    tracing::info!(%bind, "control plane listening");

    let drain_rx = shutdown_rx.clone();
    // `into_make_service_with_connect_info` exposes the peer socket address to
    // handlers via `ConnectInfo<SocketAddr>` — the invite-preview rate limiter's
    // client-IP resolver needs the real peer to decide whether to believe XFF.
    let serve = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        wait_for_shutdown(drain_rx).await;
        tracing::info!("shutting down — releasing node leases");
        // Nodes we own reconnect elsewhere and re-lease; mark them offline
        // until they do so schedulers don't route to dead sockets.
        let _ = shutdown_db
            .exec(
                &format!(
                    "UPDATE nodes SET status = 'offline', updated_at = {now},
                owning_instance_id = NULL, lease_expires_at = NULL
             WHERE owning_instance_id = $1",
                    now = type_mapping(shutdown_db.engine()).now()
                ),
                params![instance],
            )
            .await;
    });

    // Bound the drain. `with_graceful_shutdown` waits for every in-flight
    // request with no ceiling; one hung handler would otherwise hold the process
    // until Kubernetes' own grace period expires and SIGKILLs it. Race the drain
    // against a timer that starts when the signal fires: whichever finishes
    // first, we exit 0 — either drained cleanly, or forced out after the grace.
    let timer_rx = shutdown_rx.clone();
    tokio::select! {
        res = serve => res?,
        _ = async move {
            wait_for_shutdown(timer_rx).await;
            tokio::time::sleep(grace).await;
        } => {
            tracing::warn!(grace_secs = grace.as_secs(), "grace period elapsed — forcing shutdown");
        }
    }
    Ok(())
}

/// Bind the browser door and the agent door, in that order, before either is
/// served (MAIN-285).
///
/// Returning both listeners together is the point: a caller cannot serve one
/// door and then discover the other is unavailable, which is the state that
/// makes a fleet flap rather than back off. If the second bind fails, the first
/// listener is dropped on the error path and its socket closes with it.
///
/// The browser door is first because it is the contended one — everything in a
/// deployment talks to it, so it is the port a stray process is holding.
async fn bind_doors(
    bind: &str,
    agent_bind: &str,
) -> anyhow::Result<(tokio::net::TcpListener, tokio::net::TcpListener)> {
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        // Without this the failure reads only `Address already in use (os error
        // 98)`: no port, no door, nothing an operator can act on. That is
        // exactly how a week-long flap went undiagnosed.
        .with_context(|| format!("cannot bind the control plane port {bind}"))?;
    let agent_listener = tokio::net::TcpListener::bind(agent_bind)
        .await
        .with_context(|| format!("cannot bind the agent port {agent_bind}"))?;
    Ok((listener, agent_listener))
}

/// Resolve on the first termination signal. Kubernetes sends **SIGTERM** on pod
/// shutdown, so listening only for SIGINT (Ctrl-C) would leave a pod hanging
/// until the kill-grace timeout turned into a SIGKILL mid-request. Both map to
/// the same graceful drain here.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = term.recv() => tracing::info!("received SIGTERM"),
            _ = int.recv() => tracing::info!("received SIGINT"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// A future that completes when the shutdown watch flips true. A dropped sender
/// (should not happen before shutdown) also completes it — fail toward draining.
async fn wait_for_shutdown(mut rx: tokio::sync::watch::Receiver<bool>) {
    if *rx.borrow_and_update() {
        return;
    }
    let _ = rx.wait_for(|flagged| *flagged).await;
}

#[cfg(test)]
mod tests {
    use super::{bind_doors, wait_for_shutdown};
    use std::io::{BufRead, Read, Write};
    use std::time::Duration;

    /// How many times a port-using test re-rolls before it calls the failure
    /// real. Losing the allocate-then-bind race is a chance event; losing it
    /// this many times running is a bug, not bad luck.
    const PORT_ATTEMPTS: usize = 20;

    /// A free port, released before the call under test uses it.
    ///
    /// The window between "this was free" and "the code under test bound it"
    /// cannot be closed from here, and no amount of locking closes it either:
    /// `bind_doors` takes an address, so the port has to be *free* for it to
    /// bind, and a port nobody holds is a port any process on the machine can
    /// take. MAIN-285's mutex serialised the tests in this binary and left that
    /// standing, which is why a sibling test binary binding `:0` still reddened
    /// `both_doors_come_back_together` with `Address already in use` on CI
    /// (MAIN-668) — the mutex was never in either process's way.
    ///
    /// So the port is not defended, it is re-rolled: every caller runs its
    /// attempt inside a bounded loop, treats an unexpected [`addr_in_use`] as
    /// "lost the race" rather than as a finding, and allocates again. That
    /// tolerates losing to anything — a sibling binary, another test in this
    /// one, a stray process — because it never assumes it won.
    async fn free_port() -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        addr.to_string()
    }

    /// True when the failure is somebody else already holding the port, which
    /// is the one failure a test re-rolls instead of reporting.
    fn addr_in_use(err: &anyhow::Error) -> bool {
        err.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::AddrInUse)
        })
    }

    fn chain_of(err: &anyhow::Error) -> String {
        err.chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(": ")
    }

    /// Every attempt lost the race, which stops being luck and starts being a
    /// bug — so say which one it would be rather than reporting the raw
    /// `Address already in use` that took MAIN-285 a week to read.
    fn out_of_attempts(what: &str) -> ! {
        panic!(
            "{PORT_ATTEMPTS} attempts all lost the port: {what}. Either something \
             on this machine is taking loopback ports as fast as they are \
             allocated, or the behaviour under test regressed."
        )
    }

    /// MAIN-285: the flap. When the control plane cannot take its browser port,
    /// it must NOT have opened the agent door on the way there — a node that
    /// completes a handshake against a process which is about to exit is a node
    /// that reconnects a second later, forever, and never holds a session long
    /// enough to push its capabilities.
    #[tokio::test]
    async fn a_boot_that_cannot_take_the_browser_port_never_opens_the_agent_door() {
        for _ in 0..PORT_ATTEMPTS {
            // Something else already owns the browser port — a previous server
            // that outlived its restart, which is exactly the dev-loop case.
            // Held for the whole attempt, so this port is never a race.
            let squatter = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let taken = squatter.local_addr().unwrap().to_string();
            let agent = free_port().await;

            let err = bind_doors(&taken, &agent)
                .await
                .expect_err("the browser port is taken, so the boot must fail");

            // The failure names the port. `Address already in use (os error 98)`
            // alone is what made this take a week to find.
            let chain = chain_of(&err);
            assert!(
                chain.contains(&taken),
                "the error must name the contended port, got: {chain}"
            );

            // The agent door is still shut: binding it now succeeds, which it
            // could not do if the failed boot had left a listener on it. An
            // `AddrInUse` here is ambiguous — a doomed boot that opened the
            // door, or a stranger that took the released port — so re-roll,
            // and let running out of attempts be what reports the bug.
            match tokio::net::TcpListener::bind(&agent).await {
                Ok(_) => return,
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
                Err(e) => panic!("the agent door must be free, got: {e}"),
            }
        }
        out_of_attempts("the agent door was occupied after every doomed boot")
    }

    /// The other order still fails loudly, and leaves nothing behind.
    #[tokio::test]
    async fn a_contended_agent_port_fails_the_boot_and_releases_the_browser_port() {
        for _ in 0..PORT_ATTEMPTS {
            let browser = free_port().await;
            let squatter = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let taken_agent = squatter.local_addr().unwrap().to_string();

            let err = bind_doors(&browser, &taken_agent)
                .await
                .expect_err("the agent port is taken, so the boot must fail");
            // The browser door is bound first, so a failure naming *it* is this
            // attempt losing the race, not the behaviour under test. Matched on
            // `bind_doors`' whole context phrase rather than on the address
            // alone: this predicate decides whether a failure is reported or
            // discarded, and a bare address is a substring test —
            // `127.0.0.1:4444` sits inside `127.0.0.1:44445`.
            let chain = chain_of(&err);
            if addr_in_use(&err)
                && chain.contains(&format!("cannot bind the control plane port {browser}"))
            {
                continue;
            }
            assert!(
                chain.contains(&taken_agent),
                "the error must name the contended agent port, got: {chain}"
            );

            // The browser listener bound first must have been dropped with the
            // error, not leaked.
            match tokio::net::TcpListener::bind(&browser).await {
                Ok(_) => return,
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
                Err(e) => panic!("the browser port must be released, got: {e}"),
            }
        }
        out_of_attempts("the browser port was still held after every failed boot")
    }

    /// The happy path hands back both doors, so the caller can serve them
    /// together rather than one at a time.
    #[tokio::test]
    async fn both_doors_come_back_together() {
        for _ in 0..PORT_ATTEMPTS {
            let browser = free_port().await;
            let agent = free_port().await;
            match bind_doors(&browser, &agent).await {
                Ok((a, b)) => {
                    assert_eq!(a.local_addr().unwrap().to_string(), browser);
                    assert_eq!(b.local_addr().unwrap().to_string(), agent);
                    return;
                }
                Err(e) if addr_in_use(&e) => continue,
                Err(e) => panic!("both doors must bind, got: {e:#}"),
            }
        }
        out_of_attempts("a freshly allocated port was taken before every bind")
    }

    /// The variable that turns this same binary into the squatter below, the
    /// line it prints once the port is really its, and the name the parent
    /// runs it by.
    const SQUAT_ENV: &str = "NOOK_TEST_SQUAT_ADDR";
    const SQUAT_READY: &str = "nook-test-squatter-bound";
    const SQUAT_TEST: &str = "tests::hold_the_squatted_port";

    /// Not a test — the child half of [`a_port_stolen_by_another_process_is_re_rolled`],
    /// run by name out of this same binary. It takes the port the parent names
    /// and holds it until the parent closes its stdin.
    ///
    /// `#[ignore]` keeps it out of an ordinary run, and an absent variable
    /// makes it a no-op rather than a hang for anyone running the ignored set
    /// by hand.
    #[test]
    #[ignore = "the child process of a_port_stolen_by_another_process_is_re_rolled"]
    fn hold_the_squatted_port() {
        let Ok(addr) = std::env::var(SQUAT_ENV) else {
            return;
        };
        let listener = std::net::TcpListener::bind(&addr).expect("the squatter takes the port");
        println!("{SQUAT_READY}");
        std::io::stdout().flush().unwrap();
        let mut sink = String::new();
        let _ = std::io::stdin().read_to_string(&mut sink);
        drop(listener);
    }

    /// Another PROCESS holding a port.
    ///
    /// A thread would prove nothing: the collision MAIN-668 is about is a
    /// *sibling test binary*, which is exactly what an in-process mutex cannot
    /// reach — so the reproduction has to leave the process to be the failure
    /// it claims to reproduce. Re-running this binary is
    /// `nook-db/tests/single_instance_crash.rs`'s trick, and for its reason:
    /// no fixture binary to build or keep in sync. It is released by closing
    /// the child's stdin rather than by killing it after a sleep, so a stray
    /// child cannot outlive the suite holding a port.
    struct Squatter {
        child: std::process::Child,
        // Kept so the child never writes into a closed pipe while it is
        // holding the port for us.
        _stdout: std::io::BufReader<std::process::ChildStdout>,
    }

    /// How long the parent waits to hear that the child holds the port.
    /// Generous for spawning a test binary, and finite on purpose: everything
    /// else here turns a stuck state into a sentence, and the handshake was the
    /// one path that could instead block forever.
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

    impl Squatter {
        /// Spawn the child on `addr` and return once it has *actually bound* —
        /// handshaked over stdout rather than slept on, so the reproduction is
        /// deterministic. `None` when the child lost the very race being
        /// reproduced, which is the caller's cue to allocate another port; a
        /// child that neither binds nor exits inside [`HANDSHAKE_TIMEOUT`]
        /// panics by name rather than hanging.
        fn hold(addr: &str) -> Option<Self> {
            let exe = std::env::current_exe().expect("this test binary's own path");
            let mut child = std::process::Command::new(exe)
                .args(["--exact", SQUAT_TEST, "--ignored", "--nocapture"])
                .env(SQUAT_ENV, addr)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("spawning the squatter process");

            // Read on a thread so the wait can be bounded: `read_line` blocks
            // the tokio worker that called this, so a child that goes quiet
            // would hang the test instead of failing it.
            let stdout = child.stdout.take().expect("the squatter's stdout is piped");
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut reader = std::io::BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => return,
                        // `contains`, never equality. Under `--nocapture` with
                        // one test thread libtest writes `test <name> ... `
                        // with NO trailing newline before running the test, so
                        // the ready token arrives on the end of that line:
                        //   test tests::hold_the_squatted_port ... nook-test-squatter-bound
                        // Equality missed it and both sides then blocked
                        // forever — the child in `read_to_string`, the parent
                        // in `read_line`. `single_instance_crash.rs` matches
                        // this way for exactly this reason.
                        Ok(_) if line.contains(SQUAT_READY) => {
                            let _ = tx.send(reader);
                            return;
                        }
                        Ok(_) => {}
                    }
                }
            });

            match rx.recv_timeout(HANDSHAKE_TIMEOUT) {
                Ok(reader) => Some(Self {
                    child,
                    _stdout: reader,
                }),
                // The child exited without ever reporting the port: it lost the
                // same race being reproduced, so the caller re-rolls.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    None
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "the squatter never reported holding {addr} within \
                         {HANDSHAKE_TIMEOUT:?} — the handshake is broken, which \
                         without this bound would have been an indefinite hang"
                    )
                }
            }
        }
    }

    impl Drop for Squatter {
        fn drop(&mut self) {
            // Closing stdin is the release signal; the child drops the listener
            // and exits on its own.
            drop(self.child.stdin.take());
            let _ = self.child.wait();
        }
    }

    /// MAIN-668 AC-2: the fix proven against the failure mode rather than
    /// observed to be green. Another process occupies the allocate-then-bind
    /// window deliberately — the collision `PORT_LOCK` was never able to see —
    /// and the attempt still arrives at two bound doors.
    #[tokio::test]
    async fn a_port_stolen_by_another_process_is_re_rolled() {
        let (stolen, _squatter) = {
            let mut held = None;
            for _ in 0..PORT_ATTEMPTS {
                let addr = free_port().await;
                if let Some(squatter) = Squatter::hold(&addr) {
                    held = Some((addr, squatter));
                    break;
                }
            }
            held.expect("the squatter process must be able to take one allocated port")
        };

        // Un-re-rolled, this is the whole of CI run 32800414774: a port this
        // process allocated, bound by someone else before the code under test
        // could take it.
        let err = bind_doors(&stolen, &free_port().await)
            .await
            .expect_err("a stolen port cannot be bound");
        assert!(
            addr_in_use(&err),
            "the reproduction must be the real failure mode, got: {}",
            chain_of(&err)
        );

        // Re-rolled, the attempt walks off the stolen port onto a fresh one.
        // The first attempt IS the stolen port, so the recovery is exercised
        // rather than waited for.
        let mut attempts = 0;
        for _ in 0..PORT_ATTEMPTS {
            attempts += 1;
            let browser = if attempts == 1 {
                stolen.clone()
            } else {
                free_port().await
            };
            let agent = free_port().await;
            match bind_doors(&browser, &agent).await {
                Ok((a, b)) => {
                    assert!(attempts > 1, "the first attempt must have been the theft");
                    assert_ne!(browser, stolen, "the re-roll must pick a different port");
                    assert_eq!(a.local_addr().unwrap().to_string(), browser);
                    assert_eq!(b.local_addr().unwrap().to_string(), agent);
                    return;
                }
                Err(e) if addr_in_use(&e) => continue,
                Err(e) => panic!("both doors must bind once the port is re-rolled: {e:#}"),
            }
        }
        out_of_attempts("the re-roll never got past the stolen port")
    }

    #[tokio::test]
    async fn wait_for_shutdown_resolves_when_the_flag_flips() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let waiter = tokio::spawn(wait_for_shutdown(rx));
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("shutdown future resolves once the flag flips")
            .unwrap();
    }

    #[tokio::test]
    async fn wait_for_shutdown_returns_at_once_if_already_flagged() {
        // A listener that clones the receiver AFTER the signal already fired
        // must still drain immediately, not hang waiting for a change.
        let (_tx, rx) = tokio::sync::watch::channel(true);
        tokio::time::timeout(Duration::from_millis(100), wait_for_shutdown(rx))
            .await
            .expect("an already-set shutdown returns without waiting");
    }

    // The mechanism SIGTERM drives: an axum server wired to the watch stops
    // accepting and its `serve` future returns once the flag flips. This is the
    // behavioural half of AC-3 (graceful drain) without raising a real signal
    // into the shared test process.
    #[tokio::test]
    async fn a_wired_server_drains_and_stops_accepting_on_shutdown() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::watch::channel(false);
        let router = axum::Router::new().route(
            "/livez",
            axum::routing::get(nook_control::routes::health::livez),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(wait_for_shutdown(rx))
                .await
                .unwrap();
        });

        // Accepting before the signal.
        tokio::net::TcpStream::connect(addr)
            .await
            .expect("server accepts before shutdown");

        // After the signal, the serve future completes on its own.
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server drains and returns after shutdown is signalled")
            .unwrap();
    }
}
