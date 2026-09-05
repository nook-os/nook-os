//! Driving a runtime's own login with pipes instead of a terminal (MAIN-650).
//!
//! The problem this solves: a Claude subscription credential can only be minted
//! by `claude` — it is that CLI's OAuth client doing a PKCE exchange, so the
//! control plane has no client to be and its device flow cannot produce one.
//! Until now that meant a tmux session in front of a person for what is, in
//! substance, "open this link, paste back what it gives you".
//!
//! It does not need a terminal. Verified against `claude` 2.1.261 with stdin
//! closed and no pty:
//!
//! ```text
//! Opening browser to sign in…
//! If the browser didn't open, visit: https://claude.com/cai/oauth/authorize?…
//! Paste code here if prompted >
//! ```
//!
//! So the node runs it, scrapes the URL off stdout, and reports it. The UI
//! renders a link and a box; the code comes back down and goes to the child's
//! stdin. Nothing here invents an OAuth parameter — the URL is whatever the
//! runtime printed, carrying its own client id and scopes.
//!
//! The command is the ALLOWLISTED one for that runtime
//! ([`crate::runtime_auth::managed_login_args`]), keyed by name. Nothing from
//! the wire reaches a shell.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, Result};
use nook_proto::NodeToControl;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Flows this node is running, by the control plane's id. The value is how to
/// reach the child's stdin; dropping it is what ends the flow.
fn flows() -> &'static Mutex<HashMap<Uuid, mpsc::Sender<String>>> {
    static F: OnceLock<Mutex<HashMap<Uuid, mpsc::Sender<String>>>> = OnceLock::new();
    F.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The first `https://` token on a line, if any.
///
/// Deliberately loose about the words around it: the runtime's wording is not
/// ours and a phrasing change must not silently stop the flow working. What is
/// strict is the scheme — an `http://` link in an auth prompt is not something
/// to hand somebody.
pub fn scrape_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    Some(rest[..end].trim_end_matches(['.', ',', ')']).to_string())
}

/// Whether this line is the runtime asking for the code to be pasted back.
pub fn wants_code(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("paste") && l.contains("code")
}

/// Run one login to completion, reporting through `tx`.
///
/// Spawned by the caller: this awaits the child, and a node-message handler
/// that awaited it would stall the socket's only reader.
pub async fn run(flow_id: Uuid, runtime: String, tx: mpsc::Sender<NodeToControl>) {
    let finish = |error: Option<String>| NodeToControl::ManagedLoginFinished {
        flow_id,
        runtime: runtime.clone(),
        error,
    };
    if let Err(e) = drive(flow_id, &runtime, &tx).await {
        flows().lock().expect("flows").remove(&flow_id);
        tx.send(finish(Some(e.to_string()))).await.ok();
        return;
    }
    flows().lock().expect("flows").remove(&flow_id);

    // A login the runtime does not then accept is a FAILED login. Saying
    // otherwise would tell an operator the fleet is authorized when it is not.
    if crate::runtime_auth::is_authorized(&runtime) {
        tx.send(finish(None)).await.ok();
    } else {
        tx.send(finish(Some(
            "the login command finished but the runtime still reports not authorized".into(),
        )))
        .await
        .ok();
    }

    // Re-probe: this pushes the fresh state AND, on a Pod executor, publishes
    // the new credential into the Secret job Pods read.
    let profiles = crate::runtime_auth::probe_all();
    tx.send(NodeToControl::RuntimeAuthStatus { profiles })
        .await
        .ok();
}

async fn drive(flow_id: Uuid, runtime: &str, tx: &mpsc::Sender<NodeToControl>) -> Result<()> {
    let args = crate::runtime_auth::managed_login_args(runtime)
        .ok_or_else(|| anyhow!("runtime `{runtime}` has no terminal-free login"))?;

    let mut child = Command::new(runtime)
        .args(args.split_whitespace())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow!("cannot run `{runtime} {args}`: {e}"))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("the login process has no stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("the login process has no stdout"))?;

    let (code_tx, mut code_rx) = mpsc::channel::<String>(1);
    flows().lock().expect("flows").insert(flow_id, code_tx);

    // Feed whatever the operator pastes to the child, for as long as it lives.
    tokio::spawn(async move {
        while let Some(code) = code_rx.recv().await {
            if stdin
                .write_all(format!("{}\n", code.trim()).as_bytes())
                .await
                .is_err()
            {
                break;
            }
            let _ = stdin.flush().await;
        }
    });

    // Read BYTES, not lines. The paste prompt is a PROMPT — `claude` writes
    // "Paste code here if prompted > " with no trailing newline and then waits,
    // so a line reader holds it in the buffer forever and the operator gets a
    // link with nowhere to put the answer. That is exactly what happened.
    let mut reader = BufReader::new(stdout);
    let mut buf = [0u8; 1024];
    let mut seen = String::new();
    let mut sent_url: Option<String> = None;
    let mut asked = false;
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        seen.push_str(&String::from_utf8_lossy(&buf[..n]));

        if sent_url.is_none() {
            if let Some(u) = scrape_url(&seen) {
                sent_url = Some(u);
            }
        }
        // The prompt may arrive in the same read as the URL or a later one, and
        // either way it never ends a line.
        asked = asked || wants_code(&seen);

        if let Some(url) = sent_url.clone() {
            tx.send(NodeToControl::ManagedLoginPrompt {
                flow_id,
                runtime: runtime.to_string(),
                url,
                wants_code: asked,
            })
            .await
            .ok();
        }
        // Unbounded growth would be a leak on a chatty runtime; the two things
        // looked for are near the start and never split across this much.
        if seen.len() > 64 * 1024 {
            seen.drain(..32 * 1024);
        }
    }

    let status = child.wait().await?;
    if !status.success() {
        return Err(anyhow!("`{runtime} {args}` exited with {status}"));
    }
    Ok(())
}

/// Hand a pasted code to a running flow. `false` when there is no such flow —
/// it finished, expired, or belongs to another node.
pub fn submit_code(flow_id: Uuid, code: String) -> bool {
    let sender = flows().lock().expect("flows").get(&flow_id).cloned();
    match sender {
        Some(tx) => tx.try_send(code).is_ok(),
        None => false,
    }
}

/// Drop a flow. The child is killed by `kill_on_drop` when its task unwinds.
pub fn cancel(flow_id: Uuid) {
    flows().lock().expect("flows").remove(&flow_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact line `claude` 2.1.261 prints with no TTY.
    #[test]
    fn the_authorize_url_is_read_off_the_runtimes_own_output() {
        let line = "If the browser didn't open, visit: https://claude.com/cai/oauth/authorize?code=true&client_id=abc&scope=user%3Ainference";
        assert_eq!(
            scrape_url(line).as_deref(),
            Some("https://claude.com/cai/oauth/authorize?code=true&client_id=abc&scope=user%3Ainference"),
            "the whole query survives — dropping it would drop the PKCE challenge"
        );
        // Wording is the runtime's and may change; the scheme is ours to insist on.
        assert_eq!(scrape_url("visit http://claude.com/x"), None);
        assert_eq!(scrape_url("Opening browser to sign in…"), None);
    }

    /// The whole reason this reads bytes: the runtime's real output arrives as
    /// one unterminated chunk, and a line reader sees the URL but never the
    /// prompt — a link with nowhere to paste the answer.
    #[test]
    fn the_prompt_is_found_even_though_it_ends_no_line() {
        let chunk = "Opening browser to sign in…\n\
                     If the browser didn't open, visit: https://claude.com/cai/oauth/authorize?code=true\n\
                     Paste code here if prompted > ";
        assert!(
            !chunk.ends_with('\n'),
            "the prompt is unterminated, as it is live"
        );
        assert_eq!(
            scrape_url(chunk).as_deref(),
            Some("https://claude.com/cai/oauth/authorize?code=true")
        );
        assert!(wants_code(chunk), "the box has to open on this");

        // And the URL alone, before the prompt arrives, must not claim it.
        let partial = "If the browser didn't open, visit: https://claude.com/x\n";
        assert!(scrape_url(partial).is_some());
        assert!(!wants_code(partial));
    }

    #[test]
    fn the_paste_prompt_is_what_asks_for_the_box() {
        assert!(wants_code("Paste code here if prompted > "));
        assert!(wants_code("paste the CODE"));
        assert!(!wants_code("Opening browser to sign in…"));
        assert!(!wants_code("If the browser didn't open, visit: https://x"));
    }

    #[test]
    fn a_code_for_an_unknown_flow_is_refused_rather_than_dropped() {
        assert!(!submit_code(Uuid::nil(), "xyz".into()));
    }
}
