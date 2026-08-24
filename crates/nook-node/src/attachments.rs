//! `nook issues attach|attachments|download|detach` — what a ticket carries, on
//! the reader's own terms (MAIN-534, MAIN-610).
//!
//! A ticket is the whole contract an agent is handed, and once files can hang
//! off one, an agent that cannot see them is reading an incomplete brief. The
//! shape here is deliberately two steps: **attachments** says what is there,
//! **download** pulls the one file the reader decided was worth reading.
//! Nothing downloads by itself and nothing is inlined into the ticket body — a
//! run never pays for a file it did not ask for (NG-2, NG-3).
//!
//! **These verbs live under `issues`, not under a group of their own**
//! (MAIN-610). An attachment is never a free-standing resource — it only ever
//! hangs on a card or on one of that card's comments — and `docs/cli-style.md`
//! says to prefer the existing group over a convenient prefix. The old
//! `nook attachments …` spelling still routes here for one release, hidden and
//! deprecated, because loop agents are running right now with it in their
//! skill text.
//!
//! **`attach` is one command over two endpoints** (MAIN-594). Putting a file on
//! a card is an upload and then a join — `POST /user-content`, then `POST
//! /tasks/{key}/attachments` with the id it hands back — and asking a person to
//! type both, copying a uuid between them, is why there was no CLI for this at
//! all. A pure client: neither endpoint changes (NG-1).
//!
//! **Reading is machine work; writing is a person's.** `attachments` and
//! `download` answer a node token, because an agent reading its brief is the
//! case they exist for. The three write routes all call `require_user`, so
//! `attach` and `detach` refuse a machine credential here, by name, rather than
//! letting it become a 403 from somewhere the reader cannot see.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::cli::Client;
use crate::style;

const UPLOAD: &str = "/api/v1/user-content";

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
    for line in render(task, &rows, &[]) {
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

/// The block `nook issues get` prints and `nook issues attachments` prints,
/// spelled once so the two can never disagree about a size or an id.
///
/// Each row is named by its ADDRESS — `MAIN-42/shot.png`, the card followed by
/// the filename — because that is what the next command takes, so what a person
/// copies out of this listing works verbatim (MAIN-610 AC-2). The id is printed
/// beside it and resolves too, which is what keeps a script holding one working.
///
/// `comments` is the ticket's comment array when the caller has one, purely so
/// a file on a comment can say whose comment. Pass an empty slice and it still
/// says *on a comment* — which is the fact that matters.
pub fn render(task: &str, rows: &[Value], comments: &[Value]) -> Vec<String> {
    let mut out = vec![style::dim(&format!("── {} attachment(s)", rows.len()))];
    for r in rows {
        let id = r["id"].as_str().unwrap_or("—");
        let name = r["filename"].as_str().unwrap_or("(unnamed)");
        let ct = r["content_type"].as_str().unwrap_or("—");
        let size = nook_types::human_size(r["size_bytes"].as_i64().unwrap_or(0));
        let mut line = format!("  {id}  {task}/{name}  {ct}  {size}");
        if let Some(on) = comment_note(r, comments) {
            line.push_str(&format!("  {}", style::dim(&on)));
        }
        out.push(line);
    }
    out.push(style::dim("   read one with `nook issues download <ADDR>`"));
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

/// How the caller named the one attachment a verb acts on (MAIN-610 AC-2).
///
/// Two address spaces that cannot overlap: a uuid is not a shape anyone types
/// by accident, and a card key holds no `/`. So the argument decides which it
/// is, and neither form has to be flagged.
#[derive(Debug, PartialEq)]
enum Address<'a> {
    Id(&'a str),
    OnCard { task: &'a str, filename: &'a str },
}

/// The two forms, or the refusal that names both.
///
/// Split at the FIRST `/`: a key never contains one, and a filename written
/// before uploads normalised them still can — so taking the head as the card
/// keeps such a file reachable instead of resolving a name nobody typed.
fn parse_address(addr: &str) -> Result<Address<'_>> {
    if uuid::Uuid::parse_str(addr).is_ok() {
        return Ok(Address::Id(addr));
    }
    match addr.split_once('/') {
        Some((task, filename)) if !task.is_empty() && !filename.is_empty() => {
            Ok(Address::OnCard { task, filename })
        }
        _ => bail!(
            "\"{addr}\" is neither an attachment id nor an address — name one as \
             MAIN-42/shot.png, exactly as `nook issues attachments` prints it"
        ),
    }
}

/// The one row of `rows` a filename names, refused by name when it is not there.
///
/// Scoped to the whole THREAD, because that is what the listing shows: a file on
/// a comment is addressed by the card carrying that comment, which is the only
/// address a reader of the listing has.
///
/// A card may hold two files of one name — `--replace` exists precisely because
/// nothing stops it — so an ambiguous name is refused with the ids rather than
/// resolved to whichever came back first.
fn pick_by_filename<'a>(rows: &'a [Value], task: &str, filename: &str) -> Result<&'a Value> {
    let hits: Vec<&Value> = rows
        .iter()
        .filter(|r| r["filename"].as_str() == Some(filename))
        .collect();
    match hits.as_slice() {
        [one] => Ok(one),
        [] => bail!("no file named \"{filename}\" on {task}"),
        many => {
            let ids: Vec<&str> = many
                .iter()
                .map(|r| r["id"].as_str().unwrap_or("—"))
                .collect();
            bail!(
                "{task} carries {} files named \"{filename}\" — name the one you mean by id: {}",
                many.len(),
                ids.join(", ")
            )
        }
    }
}

/// The attachment record an address names, whichever form it took.
///
/// An id asks the endpoint that answers for one row; an address lists the
/// thread, which is the same request the listing made — so what a person copied
/// out of it resolves against exactly the rows they were looking at.
async fn resolve(client: &Client, addr: &str) -> Result<Value> {
    match parse_address(addr)? {
        Address::Id(id) => client
            .get_opt(&format!("/api/v1/attachments/{id}"))
            .await?
            .with_context(|| format!("no attachment {id} in this tenant")),
        Address::OnCard { task, filename } => {
            let rows = fetch(client, task).await?;
            pick_by_filename(&rows, task, filename).cloned()
        }
    }
}

/// Download one attachment to the working directory, or to `--out`.
pub async fn get(addr: &str, out: Option<&str>, force: bool) -> Result<()> {
    let client = Client::from_config()?;
    // Metadata first, bytes second: it is what lets `plan_write` refuse before
    // 25 MiB has crossed the wire, and it is the only place the original
    // filename comes from.
    let row = resolve(&client, addr).await?;
    let id = row["id"].as_str().unwrap_or(addr);
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

/// Put a file on a card: upload the bytes, then record the join (AC-2).
///
/// Two requests, one command. The upload answers with a content id that means
/// nothing on its own — bytes in a store with no owner — and the second request
/// is what makes it an attachment on this ticket. Anyone doing this by hand was
/// copying that uuid between two `curl`s.
pub async fn add(task: &str, file: &str, replace: bool, json_out: bool) -> Result<()> {
    let client = as_a_person("attach a file")?;
    let path = std::path::Path::new(file);
    let filename = upload_name(path)?;
    let content_type = content_type_for(&filename);
    let bytes = std::fs::read(path).with_context(|| format!("reading {file}"))?;

    if replace {
        // BEFORE the upload, deliberately: the rule is "one file of this name
        // on this card", and detaching after would mean a window in which two
        // exist and a failure that leaves the wrong one behind (AC-3).
        let on_card = fetch(&client, task).await?;
        for id in same_name_on_task(&on_card, &filename) {
            client
                .delete(&format!("/api/v1/attachments/{id}"))
                .await
                .with_context(|| format!("replacing the {filename} already on {task}"))?;
        }
    }

    let content = client
        .post_file(UPLOAD, &filename, &content_type, bytes)
        .await?;
    let content_id = content["id"]
        .as_str()
        .context("the upload answered without a content id")?;
    let row = client
        .post(
            &format!("/api/v1/tasks/{task}/attachments"),
            json!({ "user_content_id": content_id }),
        )
        .await?;

    if json_out {
        println!("{}", serde_json::to_string_pretty(&row)?);
        return Ok(());
    }
    println!(
        "{} {}  {}  {}",
        style::ok_c("✓"),
        row["id"].as_str().unwrap_or("—"),
        filename,
        style::dim(&content_type)
    );
    Ok(())
}

/// Take one attachment off, bytes and all.
///
/// The record is read before it is deleted so the refusal for an address that is
/// not this tenant's is one sentence rather than a 404 body, and so what was
/// removed can be named afterwards — `DELETE` answers 204 and cannot say.
pub async fn rm(addr: &str, json_out: bool) -> Result<()> {
    let client = as_a_person("remove an attachment")?;
    let row = resolve(&client, addr).await?;
    let id = row["id"].as_str().context("that attachment has no id")?;
    // Deleting the join deletes the content row with it — the foreign key
    // cascades — so this is one call, not two.
    client.delete(&format!("/api/v1/attachments/{id}")).await?;

    if json_out {
        println!("{}", serde_json::to_string_pretty(&row)?);
        return Ok(());
    }
    println!(
        "{} removed {}",
        style::ok_c("✓"),
        row["filename"].as_str().unwrap_or(id)
    );
    Ok(())
}

/// The one-line notice the retired `nook attachments …` spelling prints
/// (MAIN-610 AC-4).
///
/// It is an alias for ONE release, not politeness: loop agents are running with
/// the old spelling baked into skill text a fleet has not been re-taught, and a
/// hard removal breaks a build mid-run. Hidden from `--help`, so the new
/// spelling is the only one anybody discovers.
pub fn deprecated(old_verb: &str) {
    eprintln!("{}", deprecation_line(old_verb));
}

/// Which new verb replaces each old one — named, because "deprecated" without
/// the replacement is a dead end for the agent reading it.
fn deprecation_line(old_verb: &str) -> String {
    let new = match old_verb {
        "add" => "nook issues attach",
        "list" => "nook issues attachments",
        "get" => "nook issues download",
        "rm" => "nook issues detach",
        _ => "nook issues --help",
    };
    format!(
        "{} `nook attachments {old_verb}` is deprecated — use `{new}`",
        style::err("!")
    )
}

/// Which of the thread's attachments `--replace` takes off (AC-3).
///
/// Scoped to the TICKET: `fetch` answers for the whole thread, and a file of
/// the same name on somebody's comment is their file, not a previous version of
/// this one (NG-2).
fn same_name_on_task(rows: &[Value], filename: &str) -> Vec<String> {
    rows.iter()
        .filter(|r| r["parent_kind"].as_str() == Some("task"))
        .filter(|r| r["filename"].as_str() == Some(filename))
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .collect()
}

/// The name the file is uploaded under — its own, never the path to it.
///
/// A path is how the caller found the file; the name is what the card shows and
/// what `--replace` matches on, so `./shots/run.webm` and `/tmp/run.webm` are
/// the same file arriving twice.
fn upload_name(path: &std::path::Path) -> Result<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .filter(|n| !n.is_empty())
        .with_context(|| format!("{} does not name a file", path.display()))
}

/// A client that is a PERSON, or the refusal that says why (AC-6).
///
/// The three write routes call `require_user`, so a node token gets a 403 from
/// the server with no clue as to which credential was wrong. Saying it here
/// names the thing that is missing and how to get it, before a byte is read off
/// disk.
fn as_a_person(doing: &str) -> Result<Client> {
    let client = Client::from_config()?;
    person_only(client.is_user(), doing)?;
    Ok(client)
}

/// The refusal itself, decided before anything is read or sent — which is what
/// lets it be exercised without a control plane, as `plan_write` already is.
fn person_only(is_user: bool, doing: &str) -> Result<()> {
    if !is_user {
        bail!(
            "this is a machine credential, and only a person can {doing} — run `nook login` \
             to act as yourself.\n  Reading them (`list`, `get`) works either way."
        );
    }
    Ok(())
}

/// What the file's extension says it is (AC-4).
///
/// The stored type is what a future player or preview keys on, so
/// `application/octet-stream` on every upload would make a video unplayable
/// having uploaded it perfectly. The table is deliberately short — the formats
/// something actually renders — and anything not on it stays octet-stream,
/// which the serving route treats as a download.
///
/// `.svg` is left off on purpose: it is scriptable, and the serving route
/// echoes an `image/*` subtype back inline. Naming it here would be choosing to
/// run it in this origin.
fn content_type_for(filename: &str) -> String {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "webm" => "video/webm",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "zip" => "application/zip",
        "csv" => "text/csv",
        "md" => "text/markdown",
        "txt" | "log" => "text/plain",
        _ => "application/octet-stream",
    }
    .to_string()
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
        let lines = render(
            "MAIN-42",
            &[row("task", "t-1"), row("task_comment", "c-1")],
            &comments,
        );
        assert!(lines[0].contains("2 attachment(s)"));
        assert!(lines[1].contains("a-1") && lines[1].contains("2.0 KB"));
        assert!(
            !lines[1].contains("comment"),
            "a file on the ticket needs no annotation: {}",
            lines[1]
        );
        assert!(lines[2].contains("on Ada's comment"), "{}", lines[2]);
        assert!(lines.last().unwrap().contains("nook issues download"));
    }

    /// AC-2: the listing names each file by the address the next command takes,
    /// so copying a line out of it and pasting the middle column works.
    #[test]
    fn a_listing_names_each_file_by_its_address() {
        let lines = render("MAIN-42", &[row("task", "t-1")], &[]);
        assert!(lines[1].contains("MAIN-42/spec.md"), "{}", lines[1]);
        let printed = lines[1].split_whitespace().nth(1).unwrap();
        assert_eq!(
            parse_address(printed).unwrap(),
            Address::OnCard {
                task: "MAIN-42",
                filename: "spec.md"
            },
            "what the listing prints resolves verbatim"
        );
    }

    /// The comment array is a convenience, not a requirement — the CLI's own
    /// `attachments` has no comments to hand and must still say where a file
    /// hangs.
    #[test]
    fn without_the_comments_a_file_still_says_it_is_on_one() {
        let lines = render("MAIN-42", &[row("task_comment", "c-1")], &[]);
        assert!(lines[1].contains("on a comment"), "{}", lines[1]);
    }

    /// AC-2: both address forms name the same attachment, and an id keeps
    /// working for a script that kept one.
    #[test]
    fn an_address_is_either_a_uuid_or_a_card_and_a_filename() {
        let id = uuid::Uuid::now_v7().to_string();
        assert_eq!(parse_address(&id).unwrap(), Address::Id(&id));
        assert_eq!(
            parse_address("MAIN-42/shot.png").unwrap(),
            Address::OnCard {
                task: "MAIN-42",
                filename: "shot.png"
            }
        );
        // A filename written before uploads dropped their directories keeps its
        // slashes: the CARD is the head, never the last segment.
        assert_eq!(
            parse_address("MAIN-42/2026/08/notes.md").unwrap(),
            Address::OnCard {
                task: "MAIN-42",
                filename: "2026/08/notes.md"
            }
        );

        for bad in ["shot.png", "MAIN-42", "MAIN-42/", "/shot.png"] {
            let refusal = parse_address(bad).expect_err("neither form").to_string();
            assert!(refusal.contains("MAIN-42/shot.png"), "{refusal}");
        }
    }

    /// The two forms land on ONE row — the same one — and a name nothing
    /// carries is refused by that name rather than by a 404 body.
    #[test]
    fn a_name_and_an_id_resolve_to_the_same_attachment() {
        let rows = vec![row("task", "t-1"), row("task_comment", "c-1")];
        // Both rows are `spec.md`, which is the ambiguity a card can really
        // hold; narrowed to one, the name resolves.
        let refusal = pick_by_filename(&rows, "MAIN-42", "spec.md")
            .expect_err("two files of that name")
            .to_string();
        assert!(refusal.contains("2 files"), "{refusal}");
        assert!(
            refusal.contains("by id"),
            "it says how to proceed: {refusal}"
        );

        let one = vec![row("task", "t-1")];
        let hit = pick_by_filename(&one, "MAIN-42", "spec.md").unwrap();
        assert_eq!(hit["id"].as_str(), Some("a-1"), "the row an id would give");

        let missing = pick_by_filename(&one, "MAIN-42", "nope.md")
            .expect_err("not on the card")
            .to_string();
        assert!(
            missing.contains("nope.md") && missing.contains("MAIN-42"),
            "{missing}"
        );
    }

    /// AC-4: the retired spelling still routes, and every verb names the one
    /// that replaced it — "deprecated" with no replacement is a dead end for
    /// the agent reading it.
    #[test]
    fn the_old_spelling_names_what_replaced_it() {
        for (old, new) in [
            ("add", "nook issues attach"),
            ("list", "nook issues attachments"),
            ("get", "nook issues download"),
            ("rm", "nook issues detach"),
        ] {
            let line = deprecation_line(old);
            assert!(line.contains(&format!("nook attachments {old}")), "{line}");
            assert!(line.contains(new), "{line}");
            assert!(line.contains("deprecated"), "{line}");
            assert_eq!(line.lines().count(), 1, "one line, not a banner: {line}");
        }
    }

    /// AC-4: the stored type is what a player keys on, so the extension has to
    /// reach it. The two the epic turns on are `.webm` and `.png`.
    #[test]
    fn the_extension_decides_the_content_type() {
        assert_eq!(content_type_for("run.webm"), "video/webm");
        assert_eq!(content_type_for("shot.png"), "image/png");
        assert_eq!(content_type_for("SHOT.PNG"), "image/png");
        assert_eq!(
            content_type_for("archive.tar.gz"),
            "application/octet-stream"
        );
        assert_eq!(content_type_for("README"), "application/octet-stream");
        // Scriptable in this origin if the serving route echoed it back inline.
        assert_eq!(content_type_for("logo.svg"), "application/octet-stream");
    }

    #[test]
    fn a_path_uploads_under_its_own_name() {
        let name = |p: &str| upload_name(std::path::Path::new(p)).unwrap();
        assert_eq!(name("./shots/run.webm"), "run.webm");
        assert_eq!(name("/tmp/run.webm"), "run.webm");
        assert_eq!(name("run.webm"), "run.webm");
        assert!(upload_name(std::path::Path::new("/")).is_err());
    }

    /// AC-3: `--replace` is "one file of this name ON THIS CARD" — never a file
    /// of that name somebody put on a comment (NG-2).
    #[test]
    fn replace_matches_the_cards_own_files_by_name() {
        let att = |id: &str, kind: &str, name: &str| json!({"id": id, "parent_kind": kind, "parent_id": "p", "filename": name});
        let rows = vec![
            att("a-1", "task", "run.webm"),
            att("a-2", "task", "notes.md"),
            att("a-3", "task_comment", "run.webm"),
            att("a-4", "task", "run.webm"),
        ];
        assert_eq!(same_name_on_task(&rows, "run.webm"), vec!["a-1", "a-4"]);
        // Nothing matching is not an error — `--replace` then simply adds.
        assert!(same_name_on_task(&rows, "first.webm").is_empty());
    }

    /// AC-6: a node token is turned away by name, before a byte is read off
    /// disk — not left to become a 403 from a route the reader cannot see.
    #[test]
    fn a_machine_credential_is_refused_by_name() {
        let refusal = person_only(false, "attach a file")
            .expect_err("a node token cannot attach")
            .to_string();
        assert!(refusal.contains("machine credential"), "{refusal}");
        assert!(refusal.contains("nook login"), "{refusal}");
        assert!(
            refusal.contains("list") && refusal.contains("get"),
            "it says what a machine CAN still do: {refusal}"
        );
        assert!(person_only(true, "attach a file").is_ok());
    }
}
