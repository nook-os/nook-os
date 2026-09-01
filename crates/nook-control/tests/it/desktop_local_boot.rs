//! A desktop install's control plane comes up, and serves on the port the shell
//! chose (MAIN-434 AC-4/AC-5).
//!
//! This runs the REAL binary with the REAL map — `nook_desktop_env::
//! control_plane_env`, the same function the Tauri shell calls — and probes
//! `/healthz` on the chosen port. That is the whole point of the card. MAIN-396
//! shipped a boot environment that could not boot: it set no `SESSION_SECRET`,
//! so the process exited at config load, and it put the chosen port in
//! `NOOK_CONTROL_PORT`, a compose-side host-publish variable no Rust reads, so
//! the server bound `0.0.0.0:8080` and the health poll it was gating could
//! never pass. Both defects survived a green suite because the tests asserted
//! the environment MAP and nothing ever started a process.
//!
//! So a map-shaped assertion cannot replace this one: put the port back in a
//! variable the control plane does not read and this test fails, because
//! nothing answers where the shell is looking. It needs no Tauri — the shell's
//! contribution is a `Vec<(String, String)>`.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nook_desktop_env::{control_plane_env, load_or_create_secrets};

/// A port nothing holds, released before the child takes it — the shell's own
/// mechanism, race and all (`free_port` in `src-tauri/src/lib.rs`).
fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("read the chosen port")
        .port()
}

/// The app-data directory of a first-ever launch: no database, no secrets.
struct Install(PathBuf);

impl Install {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nook-desktop-boot-{}",
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&dir).expect("an app-data directory");
        Install(dir)
    }

    fn db(&self) -> PathBuf {
        self.0.join("nook.db")
    }
}

impl Drop for Install {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Kills the control plane however the test ends, including on a panic — a
/// leaked one would hold its ports and its single-instance lock for the rest of
/// the run.
struct Serving(Child);

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start the bundled control plane exactly as the shell does.
///
/// `env_clear` because the suite runs inside the dev container, where a real
/// `DATABASE_URL` is exported and would otherwise win; and the child runs from
/// a scratch directory so `dotenvy` finds no repo `.env` to fill in around us.
/// A desktop install has neither.
fn launch(install: &Install, port: u16, agent_port: u16) -> Serving {
    let secrets = load_or_create_secrets(&install.0).expect("first-launch secrets");
    let child = Command::new(env!("CARGO_BIN_EXE_nook-control"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .envs(control_plane_env(&install.db(), port, agent_port, &secrets))
        .current_dir(&install.0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run the control-plane binary");
    Serving(child)
}

/// Whatever the child managed to say, for a failure that would otherwise read
/// "nothing answered" with no reason attached.
///
/// It stops the process first: the pipes stay open for as long as it runs, so
/// reading a live child to EOF would hang the test instead of failing it.
fn diagnostics(serving: &mut Serving) -> String {
    let _ = serving.0.kill();
    let _ = serving.0.wait();
    let mut out = String::new();
    let streams = [
        serving
            .0
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read>),
        serving
            .0
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read>),
    ];
    for mut stream in streams.into_iter().flatten() {
        let mut buf = String::new();
        let _ = stream.read_to_string(&mut buf);
        out.push_str(&buf);
    }
    out
}

/// Poll until `/healthz` answers 200, or give up. Migrating and seeding a
/// virgin SQLite file is the slow part of a first launch.
async fn healthy_within(base: &str, limit: Duration) -> bool {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("http client");
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if client
            .get(format!("{base}/healthz"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

/// AC-4/AC-5: the process serves the port the shell picked.
#[tokio::test]
async fn a_local_install_serves_healthz_on_the_chosen_port() {
    let install = Install::new();
    let port = free_port();
    let mut serving = launch(&install, port, free_port());

    let base = format!("http://127.0.0.1:{port}");
    assert!(
        healthy_within(&base, Duration::from_secs(90)).await,
        "the control plane never answered {base}/healthz — the shell shows \
         \"starting…\" and then fails\n\n{}",
        diagnostics(&mut serving)
    );
}

/// AC-2: and only over loopback. A control plane on `0.0.0.0` is one the
/// coffee-shop wifi can reach.
///
/// Skipped where the machine has no routable address of its own — there is
/// nothing to be reachable from, so the check would assert nothing.
#[tokio::test]
async fn it_is_not_reachable_from_the_network() {
    let Some(lan) = routable_local_address() else {
        return;
    };
    let install = Install::new();
    let port = free_port();
    let mut serving = launch(&install, port, free_port());

    let base = format!("http://127.0.0.1:{port}");
    assert!(
        healthy_within(&base, Duration::from_secs(90)).await,
        "the control plane never came up\n\n{}",
        diagnostics(&mut serving)
    );

    let from_the_network = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::new(lan, port),
        Duration::from_secs(2),
    );
    assert!(
        from_the_network.is_err(),
        "a desktop control plane must not be reachable at {lan}:{port}"
    );
}

/// This machine's own routable address, asked of the routing table rather than
/// of DNS: a connected UDP socket sends nothing and needs nothing to be there.
fn routable_local_address() -> Option<std::net::IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("192.0.2.1:9").ok()?;
    let addr = sock.local_addr().ok()?.ip();
    (!addr.is_loopback() && !addr.is_unspecified()).then_some(addr)
}
