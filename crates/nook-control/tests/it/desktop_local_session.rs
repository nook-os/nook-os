//! A local install runs a real session on its own bundled node (MAIN-398).
//!
//! This is AC-4, and AC-4 exists because "the node appeared online" is not the
//! same claim. It starts the REAL control-plane binary on the REAL desktop boot
//! map, enrols the REAL node binary with the token that map seeded, opens a
//! session through the API a person's click reaches, types into it, and reads
//! the answer back off the tmux pane. Nothing here is a double.
//!
//! What that catches, and a map-shaped test cannot: the node joins on FIRST
//! LAUNCH, which is before anybody has claimed the instance — so it enrols with
//! no owner, and `require_person_may_use_node` refuses an owner-less node to
//! everyone. A desktop install would have shown one online node and refused
//! every session on it.
//!
//! Skipped where the machine has no `tmux` or no `git`: a node without them
//! cannot run a session by design (`nook join` says so and exits), so the test
//! would be asserting the absence of a dependency rather than anything about
//! this card.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nook_desktop_env::{control_plane_env, join_spec_toml, load_or_create_secrets, node_env};

/// A port nothing holds — the shell's own mechanism, race and all.
fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("read the chosen port")
        .port()
}

/// Is this tool here? Asked with the flag that tool actually accepts — `tmux`
/// has no `--version` and answers a usage error with exit 1, which read as
/// "absent" and skipped this whole test on a machine that had tmux all along.
fn have(tool: &str, version_flag: &str) -> bool {
    Command::new(tool)
        .arg(version_flag)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// One machine: an app-data directory AND a home directory, both scratch.
///
/// `HOME` is redirected because the node's default workspace root is under it
/// (`~/.nook/workspace/<tenant-slug>`, AC-3) and so is its tmux socket. A test
/// that used the real one would scatter checkouts through somebody's home and
/// could adopt a tmux session that was already there.
struct Machine {
    app_data: PathBuf,
    home: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!(
            "nook-local-session-{}",
            uuid::Uuid::now_v7().simple()
        ));
        let m = Machine {
            app_data: base.join("app-data"),
            home: base.join("home"),
        };
        std::fs::create_dir_all(&m.app_data).expect("an app-data directory");
        std::fs::create_dir_all(m.node_dir()).expect("a node config directory");
        std::fs::create_dir_all(&m.home).expect("a home directory");
        m
    }

    fn db(&self) -> PathBuf {
        self.app_data.join("nook.db")
    }

    /// `<app-data>/node`, never `~/.config/nook` — that path is the person's own
    /// CLI, and `nook join` overwrites `node.toml` without asking.
    fn node_dir(&self) -> PathBuf {
        self.app_data.join("node")
    }

    fn base(&self) -> PathBuf {
        self.app_data
            .parent()
            .expect("a scratch base")
            .to_path_buf()
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        // The node's tmux server outlives the node process, so it is killed by
        // name rather than left holding a shell in a directory about to vanish.
        for socket in tmux_sockets(&self.home) {
            let _ = Command::new("tmux")
                .args(["-L", &socket, "kill-server"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = std::fs::remove_dir_all(self.base());
    }
}

/// The tmux servers this machine's node created.
///
/// tmux does not put sockets in `$TMUX_TMPDIR` itself — it puts them in
/// `$TMUX_TMPDIR/tmux-<uid>/`, one directory per user, and the socket file is
/// named by `-L`.
fn tmux_sockets(home: &Path) -> Vec<String> {
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    std::fs::read_dir(home.join(format!("tmux-{uid}")))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect()
}

/// Kills a child however the test ends, including on a panic.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Running {
    /// Whatever it managed to say. Stops it first: the pipes stay open while it
    /// runs, so reading a live child to EOF would hang the test.
    fn diagnostics(&mut self) -> String {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let mut out = String::new();
        for mut stream in [
            self.0.stdout.take().map(|s| Box::new(s) as Box<dyn Read>),
            self.0.stderr.take().map(|s| Box::new(s) as Box<dyn Read>),
        ]
        .into_iter()
        .flatten()
        {
            let mut buf = String::new();
            let _ = stream.read_to_string(&mut buf);
            out.push_str(&buf);
        }
        out
    }
}

/// The `nook` binary — BUILT here if cargo has not already produced it.
///
/// Cargo exports `CARGO_BIN_EXE_*` only for the package under test, and the
/// node lives in another crate. Assuming a workspace run leaves `nook` beside
/// `nook-control` is what made this test pass locally and fail in CI: cargo
/// builds a package's real bin artifact for that package's OWN integration
/// tests, and `nook-node` has no `tests/` directory — so `cargo test
/// --workspace` compiles its `main.rs` only as a unit-test harness under
/// `deps/` and never writes `<target>/debug/nook`. A warm target directory had
/// one lying around from an earlier `cargo build`; a clean CI checkout does not.
///
/// So the requirement is stated instead of assumed. Building it costs a link on
/// the first run and nothing afterwards, and it holds however the suite is
/// invoked — `--workspace`, `-p nook-control`, or a single filtered test.
///
/// It must never degrade to a skip: this test IS AC-4's evidence, and a skip
/// would restore a green CI while leaving the acceptance criterion unproven.
fn node_binary() -> PathBuf {
    // `<target>/<profile>` — the directory cargo put THIS test's binaries in,
    // so the node lands in the same profile the run is already using.
    let profile_dir = Path::new(env!("CARGO_BIN_EXE_nook-control"))
        .parent()
        .expect("a target directory");
    let bin = profile_dir.join("nook");
    if bin.exists() {
        return bin;
    }

    let target_dir = profile_dir.parent().expect("a target directory");
    let built = Command::new(env!("CARGO"))
        .args(["build", "-p", "nook-node", "--bin", "nook"])
        .env("CARGO_TARGET_DIR", target_dir)
        .output()
        .expect("run cargo to build the node binary");
    assert!(
        built.status.success(),
        "could not build the node binary this test drives:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert!(
        bin.exists(),
        "cargo reported success but {} is not there",
        bin.display()
    );
    bin
}

/// A node command with the environment the desktop shell hands it.
///
/// `env_clear` because the suite runs inside the dev container, where a real
/// `DATABASE_URL` and `APP_ENV` are exported; a laptop has neither. `HOME` and
/// `PATH` are the two a shell genuinely needs — `tmux` and `git` are found on
/// `PATH`, and everything the node writes hangs off `HOME`.
fn node_command(m: &Machine) -> Command {
    let mut cmd = Command::new(node_binary());
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &m.home)
        // Keep tmux's sockets inside the scratch tree too, so a leaked server
        // is findable and killable and never collides with the developer's own.
        .env("TMUX_TMPDIR", &m.home)
        .envs(node_env(&m.node_dir()))
        .current_dir(&m.home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn launch_control_plane(m: &Machine, port: u16) -> Running {
    let secrets = load_or_create_secrets(&m.app_data).expect("first-launch secrets");
    let child = Command::new(env!("CARGO_BIN_EXE_nook-control"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .envs(control_plane_env(&m.db(), port, free_port(), &secrets))
        .current_dir(&m.app_data)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run the control-plane binary");
    Running(child)
}

async fn healthy_within(base: &str, limit: Duration) -> bool {
    let client = client();
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

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("http client")
}

/// Claim the instance the way a first-run person does, and keep the cookie.
///
/// Registration is not open — the first person to reach an unclaimed instance
/// becomes its owner — which is exactly the desktop's first launch.
async fn claim(base: &str) -> String {
    let res = client()
        .post(format!("{base}/api/v1/auth/local/bootstrap"))
        .json(&serde_json::json!({ "username": "owner", "password": "correct-horse-battery" }))
        .send()
        .await
        .expect("the bootstrap route answers");
    assert!(res.status().is_success(), "claiming the instance failed");
    res.headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .collect::<Vec<_>>()
        .join("; ")
}

/// Poll a JSON endpoint until `done` accepts the body, or give up.
async fn poll_until<T, F>(cookie: &str, url: &str, limit: Duration, done: F) -> Option<T>
where
    T: serde::de::DeserializeOwned,
    F: Fn(&T) -> bool,
{
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Ok(res) = client().get(url).header("cookie", cookie).send().await {
            if let Ok(body) = res.json::<T>().await {
                if done(&body) {
                    return Some(body);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    None
}

/// A paged listing's rows.
fn rows_of(page: &serde_json::Value) -> Vec<serde_json::Value> {
    page["rows"].as_array().cloned().unwrap_or_default()
}

/// AC-1 + AC-4: nothing is typed to enrol the node, and a session opened
/// through the API runs a real shell on it.
#[tokio::test]
async fn a_session_opened_on_the_bundled_node_runs_a_real_shell() {
    for (tool, flag) in [("tmux", "-V"), ("git", "--version")] {
        if !have(tool, flag) {
            // Loudly: a check that quietly did not run looks exactly like one
            // that passed.
            eprintln!("SKIPPING the local-session end-to-end test — no {tool}");
            return;
        }
    }
    let m = Machine::new();
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let mut cp = launch_control_plane(&m, port);
    assert!(
        healthy_within(&base, Duration::from_secs(120)).await,
        "the control plane never came up\n\n{}",
        cp.diagnostics()
    );

    // AC-1: the token came out of the app's own secrets file and went into the
    // control plane's environment. Nobody minted it in a UI and nobody typed it.
    let secrets = load_or_create_secrets(&m.app_data).expect("the secrets already written");
    let spec = m.node_dir().join("join.toml");
    std::fs::write(&spec, join_spec_toml(&base, &secrets.join_token)).expect("write the join spec");

    // The name is the ONE thing this test supplies that the shell does not, and
    // it is here to pin the ordering hazard rather than to be realistic: the
    // nodes listing is `ORDER BY name`, and `zz-` sorts after the `demo-box`
    // the seed writes. So "the first online node in the list" is deterministically
    // the WRONG node on every host, and a pick that reaches for `nodes[0]`
    // instead of the id the join recorded fails here as loudly as it failed in
    // CI. Before this, whether the bug showed up was decided by how the host's
    // name happened to sort — `azul` and a container id passed, a CI runner's
    // `runnervm…` did not.
    let joined = node_command(&m)
        .args([
            "join",
            "--name",
            "zz-bundled-node",
            "--config",
            &spec.display().to_string(),
        ])
        .output()
        .expect("run nook join");
    assert!(
        joined.status.success(),
        "the bundled node could not join\n\n{}{}\n\ncontrol plane:\n{}",
        String::from_utf8_lossy(&joined.stdout),
        String::from_utf8_lossy(&joined.stderr),
        cp.diagnostics()
    );
    let _ = std::fs::remove_file(&spec);

    // AC-3: the root is the node's own tenant-scoped default, under the user's
    // home — no `--workspace-root` was passed. A repo there is what the node
    // discovers and what a session then runs in.
    let node_toml: toml::Value = toml::from_str(
        &std::fs::read_to_string(m.node_dir().join("node.toml")).expect("node.toml"),
    )
    .expect("node.toml parses");
    let root = node_toml["workspace_roots"][0]
        .as_str()
        .expect("a workspace root")
        .replace('~', &m.home.display().to_string());
    assert!(
        root.starts_with(&m.home.display().to_string()),
        "the workspace root must be under the user's home, not {root}"
    );
    let repo = Path::new(&root).join("hello");
    std::fs::create_dir_all(&repo).expect("a repo directory");
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec![
            "-c",
            "user.email=a@b.c",
            "-c",
            "user.name=A",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "root",
        ],
    ] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(&args)
                .status()
                .expect("git runs")
                .success(),
            "git {args:?} failed"
        );
    }

    let mut node = Running(node_command(&m).arg("run").spawn().expect("run nook run"));

    // The person claims the instance — after the node joined, which is the
    // ordering a first launch actually has.
    let cookie = claim(&base).await;

    // THIS machine's node, named by the `node.toml` the join wrote — never
    // "whichever node is online first". The seed puts MAIN-226's Mission
    // Control demo box in this database too (`APP_ENV=desktop` is not
    // production, so `seed::run` gets that far), it is inserted `status =
    // 'online'` and the listing is `ORDER BY name` — so "any node is online"
    // is satisfied by `demo-box` before the real node has registered, and the
    // row that follows it belongs to a machine that has no checkout of
    // anything. Which node won was decided by how the host's name sorted
    // against "demo-box": `azul` passed, a CI runner's `runnervm…` did not.
    let node_id = node_toml["node_id"]
        .as_str()
        .expect("the join recorded a node id")
        .to_string();
    poll_until(
        &cookie,
        &format!("{base}/api/v1/nodes"),
        Duration::from_secs(60),
        |ns: &Vec<serde_json::Value>| {
            ns.iter()
                .any(|n| n["id"] == node_id.as_str() && n["status"] == "online")
        },
    )
    .await
    .unwrap_or_else(|| {
        panic!(
            "the bundled node ({node_id}) never came online\n\nnode:\n{}\n\ncontrol plane:\n{}",
            node.diagnostics(),
            cp.diagnostics()
        )
    });

    // The node reports what it finds in its roots and the control plane mints
    // the workspace — nothing here creates one, because on a real install
    // nothing does.
    let workspaces: serde_json::Value = poll_until(
        &cookie,
        &format!("{base}/api/v1/workspaces"),
        Duration::from_secs(90),
        |page: &serde_json::Value| rows_of(page).iter().any(|w| w["name"] == "hello"),
    )
    .await
    .unwrap_or_else(|| {
        panic!(
            "the node never reported the repo in its workspace root\n\nnode:\n{}",
            node.diagnostics()
        )
    });
    let workspace_id = rows_of(&workspaces)
        .iter()
        .find(|w| w["name"] == "hello")
        .and_then(|w| w["id"].as_str())
        .expect("the discovered workspace")
        .to_string();

    // AC-4: the click. This is the request the UI's "open a session" makes.
    let res = client()
        .post(format!("{base}/api/v1/sessions"))
        .header("cookie", &cookie)
        .json(&serde_json::json!({
            "workspace_id": workspace_id,
            "node_id": node_id,
            "runtime": "bash",
        }))
        .send()
        .await
        .expect("the sessions route answers");
    let status = res.status();
    let session: serde_json::Value = res.json().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "opening a session was refused ({status}): {session}\n\nnode:\n{}",
        node.diagnostics()
    );
    let session_id = session["id"].as_str().expect("a session id").to_string();

    // A real tmux, on this machine, holding this session.
    let tmux_session = poll_until(
        &cookie,
        &format!("{base}/api/v1/sessions/{session_id}"),
        Duration::from_secs(60),
        |s: &serde_json::Value| s["tmux_session"].is_string(),
    )
    .await
    .and_then(|s| s["tmux_session"].as_str().map(str::to_string))
    .unwrap_or_else(|| {
        panic!(
            "the session never got a terminal\n\nnode:\n{}",
            node.diagnostics()
        )
    });
    let sockets = tmux_sockets(&m.home);
    let listed = sockets
        .iter()
        .filter_map(|s| {
            Command::new("tmux")
                .args(["-L", s, "list-sessions", "-F", "#{session_name}"])
                .env("TMUX_TMPDIR", &m.home)
                .output()
                .ok()
        })
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .collect::<String>();
    assert!(
        listed.contains(&tmux_session),
        "tmux on this machine does not have {tmux_session}; it has: {listed:?} \
         (sockets {sockets:?})"
    );

    // A real shell: type into it and read the answer back off the pane.
    let marker = "nook-local-session-proof";
    let typed = client()
        .post(format!("{base}/api/v1/sessions/{session_id}/input"))
        .header("cookie", &cookie)
        .json(&serde_json::json!({ "text": format!("echo {marker}-ok") }))
        .send()
        .await
        .expect("the input route answers");
    assert!(typed.status().is_success(), "typing was refused");

    let deadline = Instant::now() + Duration::from_secs(45);
    let mut screen = String::new();
    while Instant::now() < deadline {
        if let Ok(res) = client()
            .post(format!("{base}/api/v1/sessions/{session_id}/output"))
            .header("cookie", &cookie)
            .json(&serde_json::json!({ "history_lines": 200 }))
            .send()
            .await
        {
            if let Ok(body) = res.json::<serde_json::Value>().await {
                screen = body["text"].as_str().unwrap_or_default().to_string();
                // The echoed COMMAND is on screen the moment it is typed; the
                // proof is the shell's own answer, which has no `echo ` before
                // it.
                if screen.contains(&format!("\n{marker}-ok")) {
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!(
        "the shell never answered — screen was:\n{screen}\n\nnode:\n{}",
        node.diagnostics()
    );
}
