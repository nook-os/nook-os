//! `nook attachments` — what a ticket carries, on the reader's own terms
//! (MAIN-534).
//!
//! A ticket is the whole contract an agent is handed, and once files can hang
//! off one, an agent that cannot see them is reading an incomplete brief. The
//! shape here is deliberately two steps: **list** says what is there, **get**
//! pulls the one file the reader decided was worth reading. Nothing downloads
//! by itself and nothing is inlined into the ticket body — a run never pays for
//! a file it did not ask for (NG-2, NG-3).
//!
//! Read-only, both verbs. Attaching stays a person's act (NG-1).

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::cli::Client;
use crate::style;

/// The whole thread's files: the ticket's own and every comment's.
pub async fn list(task: &str, json: bool) -> Result<()> {
    let client = Client::from_config()?;
    let rows = fetch(&client, task).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no attachments on {}", style::bold(task));
        return Ok(());
    }
    for line in render(&rows, &[]) {
        println!("{line}");
    }
    Ok(())
}

/// `GET /tasks/{key}/attachments?include=comments` — the one request that
/// answers for the ticket and its comments at once, rather than one per
/// comment.
pub async fn fetch(client: &Client, task: &str) -> Result<Vec<Value>> {
    let resp = client
        .get(&format!(
            "/api/v1/tasks/{task}/attachments?include=comments"
        ))
        .await?;
    Ok(resp.as_array().cloned().unwrap_or_default())
}

/// The block `nook task` prints and `nook attachments list` prints, spelled
/// once so the two can never disagree about a size or an id.
///
/// `comments` is the ticket's comment array when the caller has one, purely so
/// a file on a comment can say whose comment. Pass an empty slice and it still
/// says *on a comment* — which is the fact that matters.
pub fn render(rows: &[Value], comments: &[Value]) -> Vec<String> {
    let mut out = vec![style::dim(&format!("── {} attachment(s)", rows.len()))];
    for r in rows {
        let id = r["id"].as_str().unwrap_or("—");
        let name = r["filename"].as_str().unwrap_or("(unnamed)");
        let ct = r["content_type"].as_str().unwrap_or("—");
        let size = nook_types::human_size(r["size_bytes"].as_i64().unwrap_or(0));
        let mut line = format!("  {id}  {name}  {ct}  {size}");
        if let Some(on) = comment_note(r, comments) {
            line.push_str(&format!("  {}", style::dim(&on)));
        }
        out.push(line);
    }
    out.push(style::dim("   read one with `nook attachments get <id>`"));
    out
}

/// Whether this row hangs on a comment, and whose — `None` for a file on the
/// ticket itself, which needs no annotation.
fn comment_note(row: &Value, comments: &[Value]) -> Option<String> {
    if row["parent_kind"].as_str() != Some("task_comment") {
        return None;
    }
    let parent = row["parent_id"].as_str().unwrap_or_default();
    let author = comments
        .iter()
        .find(|c| c["id"].as_str() == Some(parent))
        .and_then(|c| c["author_name"].as_str());
    Some(match author {
        Some(who) => format!("on {who}'s comment"),
        None => "on a comment".to_string(),
    })
}

/// Download one attachment to the working directory, or to `--out`.
pub async fn get(id: &str, out: Option<&str>, force: bool) -> Result<()> {
    let client = Client::from_config()?;
    // Metadata first, bytes second: it is what lets `plan_write` refuse before
    // 25 MiB has crossed the wire, and it is the only place the original
    // filename comes from.
    let Some(row) = client.get_opt(&format!("/api/v1/attachments/{id}")).await? else {
        bail!("no attachment {id} in this tenant");
    };
    let filename = safe_filename(row["filename"].as_str().unwrap_or_default(), id);
    let content = row["user_content_id"]
        .as_str()
        .context("the attachment record carries no content id")?;

    let dest = plan_write(out, &filename, force)?;
    if let Some(dir) = dest.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }

    let bytes = client
        .get_bytes(&format!("/api/v1/user-content/{content}"))
        .await?;
    let size = bytes.len();
    std::fs::write(&dest, bytes).with_context(|| format!("writing {}", dest.display()))?;
    println!(
        "{} {} ({})",
        style::ok_c("✓"),
        dest.display(),
        nook_types::human_size(size as i64)
    );
    Ok(())
}

/// Where the bytes go, or the refusal — decided BEFORE a byte is fetched, so a
/// download that was never going to be written costs nothing (AC-3).
///
/// Overwriting is refused rather than confirmed interactively: this runs
/// unattended inside loop jobs as often as at a keyboard, and a prompt nobody
/// is there to answer is a hang. `--force` is the way to say it was meant.
fn plan_write(out: Option<&str>, filename: &str, force: bool) -> Result<std::path::PathBuf> {
    let dest = destination(out, filename);
    if dest.exists() && !force {
        bail!(
            "{} already exists — pass --out <PATH> to put it somewhere else, or --force to \
             overwrite it",
            dest.display()
        );
    }
    Ok(dest)
}

/// Where the bytes land. `--out` naming an existing DIRECTORY keeps the
/// original filename inside it, because `--out ./docs` is a sentence people
/// say and writing a file called `docs` over their directory is not what they
/// meant.
fn destination(out: Option<&str>, filename: &str) -> std::path::PathBuf {
    match out {
        Some(p) if std::path::Path::new(p).is_dir() => std::path::Path::new(p).join(filename),
        Some(p) => std::path::PathBuf::from(p),
        None => std::path::PathBuf::from(filename),
    }
}

/// The uploader's filename, reduced to something that can only ever name a
/// file in the directory the caller chose.
///
/// An attachment's name is whatever a browser sent, so it is not a path and
/// must not be able to become one: `../../.ssh/authorized_keys` downloaded
/// into "the working directory" would be neither. Everything up to the last
/// separator goes, and a name left as nothing, `.` or `..` falls back to the
/// id — which is unique, and is what the caller typed.
fn safe_filename(raw: &str, id: &str) -> String {
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim()
        .trim_start_matches('\u{0}');
    if base.is_empty() || base == "." || base == ".." {
        return id.to_string();
    }
    base.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_filename_can_only_name_a_file_here() {
        assert_eq!(safe_filename("spec.md", "the-id"), "spec.md");
        assert_eq!(safe_filename("../../etc/passwd", "the-id"), "passwd");
        assert_eq!(safe_filename("C:\\windows\\evil.exe", "the-id"), "evil.exe");
        assert_eq!(safe_filename("/absolute", "the-id"), "absolute");
        // Nothing usable left: the id is unique and is what the caller typed.
        assert_eq!(safe_filename("", "the-id"), "the-id");
        assert_eq!(safe_filename("..", "the-id"), "the-id");
        assert_eq!(safe_filename("some/dir/", "the-id"), "the-id");
    }

    /// AC-3: a file already there is never replaced by surprise. Decided before
    /// the bytes are asked for, which is why this is testable without a server.
    #[test]
    fn an_existing_file_is_refused_rather_than_overwritten() {
        let dir = std::env::temp_dir().join(format!("nook-att-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let taken = dir.join("spec.md");
        std::fs::write(&taken, b"the one already here").unwrap();

        let refusal = plan_write(Some(taken.to_str().unwrap()), "spec.md", false)
            .expect_err("it refuses")
            .to_string();
        assert!(refusal.contains("already exists"), "{refusal}");
        assert!(
            refusal.contains("--out"),
            "it says how to proceed: {refusal}"
        );
        assert_eq!(
            std::fs::read(&taken).unwrap(),
            b"the one already here",
            "and the refusal left the file alone"
        );

        // `--out` at a free path, and `--force` at a taken one, both plan a write.
        assert_eq!(
            plan_write(
                Some(dir.join("copy.md").to_str().unwrap()),
                "spec.md",
                false
            )
            .unwrap(),
            dir.join("copy.md")
        );
        assert_eq!(
            plan_write(Some(taken.to_str().unwrap()), "spec.md", true).unwrap(),
            taken
        );
        // `--out` at the DIRECTORY refuses too — the name inside it is taken.
        assert!(plan_write(Some(dir.to_str().unwrap()), "spec.md", false).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn out_pointing_at_a_directory_keeps_the_name() {
        let dir = std::env::temp_dir().join(format!("nook-att-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            destination(Some(dir.to_str().unwrap()), "spec.md"),
            dir.join("spec.md")
        );
        assert_eq!(
            destination(Some("/tmp/renamed.md"), "spec.md"),
            std::path::PathBuf::from("/tmp/renamed.md")
        );
        assert_eq!(
            destination(None, "spec.md"),
            std::path::PathBuf::from("spec.md")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    fn row(kind: &str, parent: &str) -> Value {
        json!({
            "id": "a-1",
            "parent_kind": kind,
            "parent_id": parent,
            "filename": "spec.md",
            "content_type": "text/markdown",
            "size_bytes": 2048,
        })
    }

    #[test]
    fn a_listing_names_the_id_the_size_and_which_comment() {
        let comments = vec![json!({"id": "c-1", "author_name": "Ada"})];
        let lines = render(&[row("task", "t-1"), row("task_comment", "c-1")], &comments);
        assert!(lines[0].contains("2 attachment(s)"));
        assert!(lines[1].contains("a-1") && lines[1].contains("2.0 KB"));
        assert!(
            !lines[1].contains("comment"),
            "a file on the ticket needs no annotation: {}",
            lines[1]
        );
        assert!(lines[2].contains("on Ada's comment"), "{}", lines[2]);
        assert!(lines.last().unwrap().contains("nook attachments get"));
    }

    /// The comment array is a convenience, not a requirement — the CLI's own
    /// `list` has no comments to hand and must still say where a file hangs.
    #[test]
    fn without_the_comments_a_file_still_says_it_is_on_one() {
        let lines = render(&[row("task_comment", "c-1")], &[]);
        assert!(lines[1].contains("on a comment"), "{}", lines[1]);
    }
}
