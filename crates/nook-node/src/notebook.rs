//! `nook notebook` — the personal notebook (MAIN-66) from a terminal (MAIN-575).
//!
//! A CLIENT over the notebook's existing endpoints and nothing more: no new
//! route and no new type (NG-3). Everything that makes this pleasant to type —
//! addressing by path, `mkdir -p` folders, append — is composed here out of the
//! same calls the browser and the MCP tools already make.
//!
//! **A path is the address; an id still works.** MAIN-574 made a name unique
//! among its siblings, which is what turns `"Nook/Ideas/2026-08-13"` into an
//! unambiguous reference; a name may not contain `/`, so a path and a uuid can
//! never be mistaken for one another.

use std::future::Future;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::cli::Client;
use crate::style;

const NOTES: &str = "/api/v1/notebook/notes";
const FOLDERS: &str = "/api/v1/notebook/folders";

/// The notebook calls the composed verbs make.
///
/// A seam rather than a convenience: `mkdir -p` and the append guard are loops
/// whose correctness is about SEQUENCE — what is re-read, and when — and this
/// is what lets them be exercised against a racing writer with no control plane
/// in the test. Everything else here talks to [`Client`] directly.
trait NotebookApi {
    fn folders(&self) -> impl Future<Output = Result<Vec<Value>>>;
    fn create_folder(
        &self,
        name: &str,
        parent: Option<&str>,
    ) -> impl Future<Output = Result<Value>>;
    fn note(&self, id: &str) -> impl Future<Output = Result<Value>>;
    /// PATCH the body, returning the status so a refusal can be read rather
    /// than only reported.
    fn write_body(&self, id: &str, body: &str) -> impl Future<Output = Result<(u16, Value)>>;
}

impl NotebookApi for Client {
    async fn folders(&self) -> Result<Vec<Value>> {
        Ok(self
            .get(FOLDERS)
            .await?
            .as_array()
            .cloned()
            .unwrap_or_default())
    }

    async fn create_folder(&self, name: &str, parent: Option<&str>) -> Result<Value> {
        self.post(FOLDERS, json!({ "name": name, "parent_id": parent }))
            .await
    }

    async fn note(&self, id: &str) -> Result<Value> {
        self.get(&format!("{NOTES}/{id}")).await
    }

    async fn write_body(&self, id: &str, body: &str) -> Result<(u16, Value)> {
        self.patch_status(&format!("{NOTES}/{id}"), json!({ "content_md": body }))
            .await
    }
}

/// A client that is a PERSON, or the refusal that says why (AC-8).
///
/// Not a nicety. A node token authenticates as its tenant's OWNER
/// (`auth::node_token_ctx` borrows their user id), so the notebook routes would
/// answer it — with that person's private notes. Refusing here, by name, keeps
/// a machine credential out of somebody's notebook and says what to do instead,
/// rather than leaving a bare 401 to interpret.
fn as_a_person() -> Result<Client> {
    let client = Client::from_config()?;
    if !client.is_user() {
        bail!(
            "the notebook is person-owned and this is a machine credential — run `nook login` \
             to act as yourself.\n  A node token speaks as the tenant owner, so it must never \
             become a way to read their notebook."
        );
    }
    Ok(client)
}

async fn all_notes(client: &Client) -> Result<Vec<Value>> {
    Ok(client
        .get(NOTES)
        .await?
        .as_array()
        .cloned()
        .unwrap_or_default())
}

// ── Addressing ───────────────────────────────────────────────────────────────

/// The segments of a slash-delimited path.
///
/// A blank segment is refused rather than skipped: `"a//b"` and `"a/"` name a
/// folder whose name is empty, which the API cannot hold (`validate_name`), so
/// dropping it silently would resolve a path the caller did not type.
fn segments(path: &str) -> Result<Vec<&str>> {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.iter().any(|p| p.is_empty()) {
        bail!("\"{path}\" has a blank path segment — a note or folder name cannot be empty");
    }
    Ok(parts)
}

/// Is this argument an id rather than a path (AC-2)? A name may not contain
/// `/`, and a uuid is not a shape anyone types by accident, so the two address
/// spaces never overlap.
fn looks_like_id(arg: &str) -> bool {
    uuid::Uuid::parse_str(arg).is_ok()
}

/// Where a walk had got to, for an error that names the segment which failed.
fn under(walked: &[&str]) -> String {
    if walked.is_empty() {
        "the notebook root".to_string()
    } else {
        format!("\"{}\"", walked.join("/"))
    }
}

/// The child of `parent` (`None` = the notebook root) named exactly `name`.
fn child_of<'a>(folders: &'a [Value], parent: Option<&str>, name: &str) -> Option<&'a Value> {
    folders
        .iter()
        .find(|f| f["name"].as_str() == Some(name) && f["parent_id"].as_str() == parent)
}

/// A folder row's id, as an error rather than as `None`.
///
/// The distinction matters mid-walk: an id that quietly became `None` reads as
/// "the notebook root" to the next segment, so a malformed row would not fail —
/// it would silently create the rest of the path in the wrong place.
fn folder_id(row: &Value) -> Result<String> {
    row["id"]
        .as_str()
        .map(str::to_string)
        .context("the notebook returned a folder with no id")
}

/// The id of the folder `target` names — a uuid, or a path walked one
/// exact-match segment at a time from the root.
///
/// Read-only: a missing level is an error naming that level, never a folder
/// conjured by a read. The `mkdir -p` half of AC-3 lives in [`ensure_folder`].
fn resolve_folder(folders: &[Value], target: &str) -> Result<String> {
    if looks_like_id(target) {
        return folders
            .iter()
            .find(|f| f["id"].as_str() == Some(target))
            .and_then(|f| f["id"].as_str().map(str::to_string))
            .with_context(|| format!("no folder {target} in your notebook"));
    }
    let mut parent: Option<String> = None;
    let mut walked: Vec<&str> = Vec::new();
    for seg in segments(target)? {
        let Some(found) = child_of(folders, parent.as_deref(), seg) else {
            bail!("no folder named \"{seg}\" in {}", under(&walked));
        };
        parent = Some(folder_id(found)?);
        walked.push(seg);
    }
    parent.context("that folder path resolved to no id")
}

/// The note summary `target` names — a uuid, or a path whose last segment is
/// the title and whose leading segments are the folder holding it.
///
/// Matched against the address the LIST prints, not by splitting the argument:
/// what a person copies out of `nook notebook list` then works verbatim. That is
/// not only tidiness — MAIN-574 refuses a `/` in a new name, but notes written
/// before it can still hold one (`Stand Up Notes/8/12/2026` is a live example),
/// and splitting on the last slash would make exactly those unreachable.
///
/// The split-and-walk below runs only to FAIL well, so a missing folder is
/// reported as the missing folder rather than as a missing note.
fn resolve_note<'a>(notes: &'a [Value], folders: &[Value], target: &str) -> Result<&'a Value> {
    if looks_like_id(target) {
        return notes
            .iter()
            .find(|n| n["id"].as_str() == Some(target))
            .with_context(|| format!("no note {target} in your notebook"));
    }
    if let Some(hit) = notes.iter().find(|n| display_path(n) == target) {
        return Ok(hit);
    }
    let segs = segments(target)?;
    let (title, dirs) = segs
        .split_last()
        .context("a note path needs at least a title")?;
    if !dirs.is_empty() {
        resolve_folder(folders, &dirs.join("/"))?;
    }
    bail!("no note titled \"{title}\" in {}", under(dirs));
}

/// How a note reads back to a human: folder path and title, or just the title
/// when it sits at the root.
fn display_path(note: &Value) -> String {
    let title = note["title"].as_str().unwrap_or("(untitled)");
    match note["path"].as_str().unwrap_or("") {
        "" => title.to_string(),
        path => format!("{path}/{title}"),
    }
}

fn note_id(note: &Value) -> Result<String> {
    note["id"]
        .as_str()
        .map(str::to_string)
        .context("the notebook returned a note with no id")
}

// ── Bodies ───────────────────────────────────────────────────────────────────

/// `--content -` is stdin, anything else is the value verbatim (AC-5).
///
/// The convention `nook set-description` follows (MAIN-470 AC-1), and for the
/// same reason: a lone `-` stored literally is how a ticket's contract once
/// became the one-character string `-`.
fn content_arg(value: &str, stdin: impl FnOnce() -> std::io::Result<String>) -> Result<String> {
    if value == "-" {
        return stdin().context("reading the content from stdin");
    }
    Ok(value.to_string())
}

fn from_stdin() -> std::io::Result<String> {
    let mut s = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
    Ok(s)
}

/// The body an append writes: what is there, one blank line, then the new block
/// (AC-4).
///
/// Trailing whitespace goes from both sides and leading newlines from the
/// addition, so twenty appends produce twenty blocks one blank line apart
/// rather than a gap that widens each round — `echo | nook notebook append` sends
/// a trailing newline every time. Indentation inside the block survives, and an
/// empty note is just the addition with no leading blank line to explain.
fn append_body(current: &str, addition: &str) -> String {
    let head = current.trim_end();
    let tail = addition.trim_start_matches(['\n', '\r']).trim_end();
    if head.is_empty() {
        return tail.to_string();
    }
    format!("{head}\n\n{tail}")
}

/// AC-7: a sealed body is zero-knowledge — the server holds ciphertext it
/// cannot open, and only the browser has the app password that opens it. So the
/// CLI says so and exits non-zero. It deliberately does not offer to prompt: a
/// passphrase typed into a process at a terminal is the very thing the seal
/// exists to avoid.
fn refuse_if_sealed(note: &Value, target: &str) -> Result<()> {
    if note["sealed"].as_bool() == Some(true) {
        bail!(
            "{target} is sealed — open it in the web UI, where your app password decrypts it. \
             The CLI never handles that password."
        );
    }
    Ok(())
}

// ── Rendering ────────────────────────────────────────────────────────────────

/// The table `nook notebook list` prints (AC-6): where the note is, when it last
/// changed, and a `sealed` marker on the ones whose body this CLI cannot show
/// (AC-7).
fn render_list(notes: &[Value]) -> Vec<String> {
    let mut out = vec![style::dim(&format!("── {} note(s)", notes.len()))];
    let paths: Vec<String> = notes.iter().map(display_path).collect();
    let width = paths.iter().map(|p| p.chars().count()).max().unwrap_or(0);
    for (note, path) in notes.iter().zip(&paths) {
        let pad = " ".repeat(width - path.chars().count());
        let when = note["updated_at"].as_str().unwrap_or("");
        let mut line = format!("  {path}{pad}  {}", style::dim(when));
        if note["sealed"].as_bool() == Some(true) {
            line.push_str(&format!("  {}", style::accent("sealed")));
        }
        out.push(line);
    }
    out
}

/// The indented folder tree `nook notebook folders` prints (AC-6), mirroring the
/// UI's `buildTree` — including its robustness rule: a folder whose parent is
/// not in the list is shown at the root rather than vanishing.
fn render_tree(folders: &[Value], notes: &[Value]) -> Vec<String> {
    let mut out = vec![style::dim(&format!("── {} folder(s)", folders.len()))];
    let known: Vec<&str> = folders.iter().filter_map(|f| f["id"].as_str()).collect();
    let roots: Vec<&Value> = folders
        .iter()
        .filter(|f| !f["parent_id"].as_str().is_some_and(|p| known.contains(&p)))
        .collect();
    for root in sorted_by_name(roots) {
        walk(root, folders, notes, 1, &mut out);
    }
    let at_root = notes.iter().filter(|n| n["folder_id"].is_null()).count();
    if at_root > 0 {
        out.push(format!(
            "  {}  {}",
            style::dim("(root)"),
            style::dim(&notes_count(at_root))
        ));
    }
    out
}

fn walk(folder: &Value, folders: &[Value], notes: &[Value], depth: usize, out: &mut Vec<String>) {
    let id = folder["id"].as_str().unwrap_or_default();
    let held = notes
        .iter()
        .filter(|n| n["folder_id"].as_str() == Some(id))
        .count();
    let mut line = format!(
        "{}{}",
        "  ".repeat(depth),
        folder["name"].as_str().unwrap_or("(unnamed)")
    );
    if held > 0 {
        line.push_str(&format!("  {}", style::dim(&notes_count(held))));
    }
    out.push(line);
    let children: Vec<&Value> = folders
        .iter()
        .filter(|f| f["parent_id"].as_str() == Some(id))
        .collect();
    for child in sorted_by_name(children) {
        walk(child, folders, notes, depth + 1, out);
    }
}

fn notes_count(n: usize) -> String {
    if n == 1 {
        "1 note".to_string()
    } else {
        format!("{n} notes")
    }
}

fn sorted_by_name(mut rows: Vec<&Value>) -> Vec<&Value> {
    rows.sort_by_key(|f| f["name"].as_str().unwrap_or("").to_lowercase());
    rows
}

fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

// ── The two loops ────────────────────────────────────────────────────────────

/// `--folder` as `mkdir -p` (AC-3): every missing level created, every existing
/// one reused in silence, so a skill can write the same path every run with no
/// special-casing. `None` back means the notebook root.
///
/// A failed create is re-checked against a fresh listing before it is reported,
/// which is what absorbs MAIN-574's 409: two runs racing on the same new path
/// is the ordinary case here, and the loser wanted exactly the folder the
/// winner just made.
async fn ensure_folder(api: &impl NotebookApi, target: &str) -> Result<Option<String>> {
    let mut folders = api.folders().await?;
    if looks_like_id(target) {
        return Ok(Some(resolve_folder(&folders, target)?));
    }
    let mut parent: Option<String> = None;
    for seg in segments(target)? {
        if let Some(found) = child_of(&folders, parent.as_deref(), seg) {
            parent = Some(folder_id(found)?);
            continue;
        }
        parent = Some(match api.create_folder(seg, parent.as_deref()).await {
            Ok(made) => folder_id(&made)?,
            Err(e) => {
                folders = api.folders().await?;
                match child_of(&folders, parent.as_deref(), seg) {
                    Some(found) => folder_id(found)?,
                    None => return Err(e.context(format!("creating the folder \"{seg}\""))),
                }
            }
        });
    }
    Ok(parent)
}

/// Read the body, append to it, write it back — aborting rather than
/// overwriting if it moved underneath (AC-4).
///
/// There is no append endpoint, so this is read-modify-write, and the API has
/// no compare-and-set for a client to lean on: `UpdateUserNote` carries no
/// version field, so the server cannot refuse a stale write the way
/// `PATCH /tasks/{key}` does. Adding one is MAIN-590's job, not this card's
/// (NG-3) — so what is left is to make the failure mode ABORT instead of
/// clobber.
///
/// Hence: read, compose, then read again immediately before the write. If the
/// body moved between those two reads, nothing is written at all and the
/// caller is told to look again. That is a real guarantee for the interval it
/// covers — the note listing, the fetch, and composing the new body — which is
/// where the time actually goes.
///
/// **The last round trip is still open**, and that is the residual race
/// `--help` names out loud: an edit landing between the check and the PATCH is
/// overwritten, and no client-only scheme can see it. Do not treat this as a
/// lock. When MAIN-590 lands, this passes `expected_updated_at` and retries on
/// a 409, and the window closes.
///
/// `updated_at` is deliberately NOT the discriminator. It moves for a rename
/// or a move as much as for a body edit, and neither of those makes the body
/// we composed from stale — aborting on one would be a refusal the caller
/// could do nothing about. It is the token MAIN-590 will send; the body is
/// what "changed underneath" means here.
async fn append_to(
    api: &impl NotebookApi,
    id: &str,
    target: &str,
    addition: &str,
) -> Result<Value> {
    let note = api.note(id).await?;
    refuse_if_sealed(&note, target)?;
    let before = body_of(&note);
    let composed = append_body(&before, addition);

    let now = api.note(id).await?;
    // Sealed since the first read: caught here rather than left to the 409 the
    // PATCH would earn, because "it was sealed while you typed" is the useful
    // sentence and the conflict body is not.
    refuse_if_sealed(&now, target)?;
    if body_of(&now) != before {
        bail!(
            "{target} changed while this append was being prepared — NOTHING was written. \
             Read it again and re-run the append."
        );
    }

    let (status, written) = api.write_body(id, &composed).await?;
    match status {
        200 => Ok(written),
        401 => bail!("unauthorized — this CLI's token was rejected"),
        other => bail!("{other} appending to {target}: {written}"),
    }
}

/// A note's plaintext body. Absent means sealed — and `""` is then the right
/// answer only because every caller here has already refused a sealed note.
fn body_of(note: &Value) -> String {
    note["content_md"].as_str().unwrap_or_default().to_string()
}

// ── Verbs ────────────────────────────────────────────────────────────────────

/// `nook notebook list [--folder <path|id>] [--json]`.
pub async fn list(folder: Option<&str>, json: bool) -> Result<()> {
    let client = as_a_person()?;
    let mut notes = all_notes(&client).await?;
    if let Some(target) = folder {
        // Resolved, never created: `--folder` on a READ says which folder to
        // show, and a listing that quietly made one would be a write nobody
        // asked for.
        let id = resolve_folder(&client.folders().await?, target)?;
        notes.retain(|n| n["folder_id"].as_str() == Some(id.as_str()));
    }
    if json {
        return print_json(&Value::Array(notes));
    }
    if notes.is_empty() {
        println!("no notes here yet — write one with `nook notebook create --title …`");
        return Ok(());
    }
    for line in render_list(&notes) {
        println!("{line}");
    }
    Ok(())
}

/// `nook notebook read <path|id> [--json]`.
pub async fn read(target: &str, json: bool) -> Result<()> {
    let client = as_a_person()?;
    let notes = all_notes(&client).await?;
    let summary = resolve_note(&notes, &client.folders().await?, target)?;
    let note = client.note(&note_id(summary)?).await?;
    refuse_if_sealed(&note, target)?;
    if json {
        return print_json(&note);
    }
    println!("{}", style::bold(&display_path(summary)));
    println!();
    println!("{}", note["content_md"].as_str().unwrap_or_default());
    Ok(())
}

/// `nook notebook create --title <t> [--folder <path|id>] [--content <text|->]`.
///
/// Repeating the identical command SUCCEEDS rather than conflicting, which is
/// the other half of what lets a skill write the same path every run (AC-3).
/// Only the identical one: a note already there under a different body is
/// refused and pointed at `append`, because overwriting on a `create` would be
/// exactly the silent loss this group is built to avoid.
pub async fn create(
    title: &str,
    folder: Option<&str>,
    content: Option<&str>,
    json: bool,
) -> Result<()> {
    let body = match content {
        Some(v) => content_arg(v, from_stdin)?,
        None => String::new(),
    };
    let client = as_a_person()?;
    let folder_id = match folder {
        Some(f) => ensure_folder(&client, f).await?,
        None => None,
    };
    let existing = all_notes(&client).await?.into_iter().find(|n| {
        n["title"].as_str() == Some(title) && n["folder_id"].as_str() == folder_id.as_deref()
    });
    if let Some(existing) = existing {
        let note = client.note(&note_id(&existing)?).await?;
        // Sealed first, because a sealed note's body is ABSENT rather than
        // empty — comparing it would read as `""` and report a note this CLI
        // cannot even see as "already there, unchanged".
        if note["sealed"].as_bool() == Some(true) {
            bail!(
                "a note titled \"{title}\" is already there and it is sealed — this CLI cannot \
                 read or change its body. Open it in the web UI, or create this one under \
                 another title."
            );
        }
        if body_of(&note) != body {
            bail!(
                "a note titled \"{title}\" is already {} and its body differs — \
                 `nook notebook append` adds to it, `nook notebook read` shows it",
                match folder {
                    Some(f) => format!("in \"{f}\""),
                    None => "at the notebook root".to_string(),
                }
            );
        }
        if json {
            return print_json(&note);
        }
        println!(
            "{} {} is already there, unchanged",
            style::ok_c("✓"),
            style::bold(&display_path(&existing))
        );
        return Ok(());
    }
    let created = client
        .post(
            NOTES,
            json!({ "title": title, "content_md": body, "folder_id": folder_id }),
        )
        .await?;
    if json {
        return print_json(&created);
    }
    println!(
        "{} created {}",
        style::ok_c("✓"),
        style::bold(&match folder {
            Some(f) => format!("{f}/{title}"),
            None => title.to_string(),
        })
    );
    Ok(())
}

/// `nook notebook append <path|id> --content <text|->`.
pub async fn append(target: &str, content: &str, json: bool) -> Result<()> {
    let addition = content_arg(content, from_stdin)?;
    if addition.trim().is_empty() {
        bail!("nothing to append — `--content` was empty");
    }
    let client = as_a_person()?;
    let notes = all_notes(&client).await?;
    let summary = resolve_note(&notes, &client.folders().await?, target)?;
    let after = append_to(&client, &note_id(summary)?, target, &addition).await?;
    if json {
        return print_json(&after);
    }
    println!(
        "{} appended to {}",
        style::ok_c("✓"),
        style::bold(&display_path(summary))
    );
    Ok(())
}

/// `nook notebook delete <path|id>`. Notes only — this card gives folders no
/// delete (NG-2), and one holding notes would take them with it.
pub async fn delete(target: &str, json: bool) -> Result<()> {
    let client = as_a_person()?;
    let notes = all_notes(&client).await?;
    let summary = resolve_note(&notes, &client.folders().await?, target)?.clone();
    client
        .delete(&format!("{NOTES}/{}", note_id(&summary)?))
        .await?;
    if json {
        return print_json(&summary);
    }
    println!(
        "{} deleted {}",
        style::ok_c("✓"),
        style::bold(&display_path(&summary))
    );
    Ok(())
}

/// `nook notebook folders [--json]` — the tree, indented (AC-6). Read-only: this
/// card gives folders no rename, move or delete (NG-2), and creation is
/// implicit in `create --folder`.
pub async fn folders(json: bool) -> Result<()> {
    let client = as_a_person()?;
    let folders = client.folders().await?;
    if json {
        return print_json(&Value::Array(folders));
    }
    if folders.is_empty() {
        println!(
            "no folders yet — `nook notebook create --folder \"A/B\" --title …` makes them as it goes"
        );
        return Ok(());
    }
    for line in render_tree(&folders, &all_notes(&client).await?) {
        println!("{line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn folder(id: &str, name: &str, parent: Option<&str>) -> Value {
        json!({ "id": id, "name": name, "parent_id": parent })
    }

    fn note(id: &str, title: &str, path: &str, folder_id: Option<&str>) -> Value {
        json!({
            "id": id,
            "title": title,
            "path": path,
            "folder_id": folder_id,
            "sealed": false,
            "updated_at": "2026-08-13T10:00:00Z",
        })
    }

    /// The tree the addressing tests walk: `Nook/Scratch/Deep`, plus a second
    /// `Nook` nested deeper so a bare name is never enough on its own.
    fn tree() -> Vec<Value> {
        vec![
            folder("f-nook", "Nook", None),
            folder("f-scratch", "Scratch", Some("f-nook")),
            folder("f-deep", "Deep", Some("f-scratch")),
            folder("f-other", "Nook", Some("f-scratch")),
        ]
    }

    #[test]
    fn a_path_resolves_one_exact_segment_at_a_time() {
        let folders = tree();
        assert_eq!(resolve_folder(&folders, "Nook").unwrap(), "f-nook");
        assert_eq!(
            resolve_folder(&folders, "Nook/Scratch/Deep").unwrap(),
            "f-deep"
        );
        // The same NAME at two depths is two folders; the path says which.
        assert_eq!(
            resolve_folder(&folders, "Nook/Scratch/Nook").unwrap(),
            "f-other"
        );
    }

    /// AC-2: an id addresses the same row a path does — and an id that is not
    /// in the notebook is an error, not a value passed through to the server.
    #[test]
    fn an_id_addresses_the_same_row_as_a_path() {
        let mut folders = tree();
        let id = "0198f2c0-0000-7000-8000-000000000000";
        folders.push(folder(id, "ById", None));
        assert_eq!(resolve_folder(&folders, id).unwrap(), id);
        assert!(resolve_folder(&tree(), id).is_err(), "unknown id refused");

        let notes = vec![note(
            "0198f2c0-0000-7000-8000-000000000001",
            "t",
            "Nook",
            Some("f-nook"),
        )];
        assert_eq!(
            resolve_note(&notes, &folders, "0198f2c0-0000-7000-8000-000000000001").unwrap()["id"],
            "0198f2c0-0000-7000-8000-000000000001"
        );
        assert_eq!(
            resolve_note(&notes, &folders, "Nook/t").unwrap()["id"],
            notes[0]["id"]
        );
    }

    #[test]
    fn a_missing_segment_says_which_one() {
        let folders = tree();
        let e = resolve_folder(&folders, "Nook/Missing/Deep")
            .expect_err("it refuses")
            .to_string();
        assert!(e.contains("\"Missing\""), "{e}");
        assert!(e.contains("\"Nook\""), "and where it looked: {e}");

        let e = resolve_folder(&folders, "Nope")
            .expect_err("it refuses")
            .to_string();
        assert!(e.contains("the notebook root"), "{e}");
    }

    #[test]
    fn a_blank_segment_is_refused_rather_than_skipped() {
        for bad in ["", "a//b", "a/", "/a"] {
            assert!(segments(bad).is_err(), "{bad} should be refused");
        }
        assert_eq!(segments("a/b").unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn a_note_resolves_by_its_folder_path_and_title() {
        let folders = tree();
        let notes = vec![
            note("n-1", "test", "Nook/Scratch/Deep", Some("f-deep")),
            note("n-2", "test", "", None),
        ];
        assert_eq!(
            resolve_note(&notes, &folders, "Nook/Scratch/Deep/test").unwrap()["id"],
            "n-1"
        );
        // The same title at the root is a different note.
        assert_eq!(resolve_note(&notes, &folders, "test").unwrap()["id"], "n-2");
    }

    /// A title written before MAIN-574 can hold a `/`. The address `list`
    /// prints still resolves it — splitting the argument on the last slash
    /// would leave those notes reachable only by id.
    #[test]
    fn a_title_with_a_slash_in_it_still_resolves() {
        let folders = tree();
        let notes = vec![note("n-1", "8/12/2026", "Nook/Scratch", Some("f-scratch"))];
        assert_eq!(display_path(&notes[0]), "Nook/Scratch/8/12/2026");
        assert_eq!(
            resolve_note(&notes, &folders, "Nook/Scratch/8/12/2026").unwrap()["id"],
            "n-1"
        );
    }

    #[test]
    fn a_note_under_a_missing_folder_names_the_folder_not_the_note() {
        let folders = tree();
        let notes = vec![note("n-1", "test", "Nook/Scratch/Deep", Some("f-deep"))];
        let e = resolve_note(&notes, &folders, "Nook/Nowhere/test")
            .expect_err("it refuses")
            .to_string();
        assert!(e.contains("\"Nowhere\""), "{e}");

        // The folder is there; the title is not.
        let e = resolve_note(&notes, &folders, "Nook/Scratch/Deep/absent")
            .expect_err("it refuses")
            .to_string();
        assert!(e.contains("\"absent\""), "{e}");
        assert!(e.contains("Nook/Scratch/Deep"), "{e}");
    }

    /// AC-4: two appends make one note with both blocks, one blank line apart —
    /// and a trailing newline from `echo` does not widen the gap.
    #[test]
    fn appending_separates_blocks_by_exactly_one_blank_line() {
        assert_eq!(append_body("", "hello\n"), "hello");
        assert_eq!(
            append_body("hello", "second block\n"),
            "hello\n\nsecond block"
        );
        let twice = append_body(&append_body("", "hello\n"), "second block\n");
        assert_eq!(twice, "hello\n\nsecond block");
        assert_eq!(
            append_body(&twice, "third\n"),
            "hello\n\nsecond block\n\nthird"
        );
        // A body already ending in blank lines does not accumulate more.
        assert_eq!(append_body("hello\n\n\n", "next"), "hello\n\nnext");
        // Indentation inside the addition survives; leading blank lines do not.
        assert_eq!(append_body("a", "\n\n    indented"), "a\n\n    indented");
    }

    #[test]
    fn a_lone_dash_is_stdin_and_never_content() {
        assert_eq!(
            content_arg("-", || Ok("from stdin\n".to_string())).unwrap(),
            "from stdin\n"
        );
        assert_eq!(
            content_arg("inline text", || panic!("stdin must not be read")).unwrap(),
            "inline text"
        );
        // Only a LONE dash: a body that merely starts with one is content.
        assert_eq!(
            content_arg("- a list item", || panic!("stdin must not be read")).unwrap(),
            "- a list item"
        );
    }

    /// AC-7: a sealed note lists with its title and a marker, and reading it is
    /// a refusal that points at the UI.
    #[test]
    fn a_sealed_note_lists_but_does_not_read() {
        let mut sealed = note("n-1", "private", "Nook", Some("f-nook"));
        sealed["sealed"] = json!(true);
        let lines = render_list(&[sealed.clone()]).join("\n");
        assert!(lines.contains("Nook/private"), "{lines}");
        assert!(lines.contains("sealed"), "{lines}");

        let e = refuse_if_sealed(&sealed, "Nook/private")
            .expect_err("it refuses")
            .to_string();
        assert!(e.contains("sealed"), "{e}");
        assert!(e.contains("web UI"), "it points at the UI: {e}");
        assert!(
            refuse_if_sealed(&note("n-2", "open", "", None), "open").is_ok(),
            "an unsealed note reads normally"
        );
    }

    #[test]
    fn the_folder_tree_is_indented_and_counts_what_each_holds() {
        let notes = vec![
            note("n-1", "test", "Nook/Scratch/Deep", Some("f-deep")),
            note("n-2", "loose", "", None),
        ];
        let body = render_tree(&tree(), &notes).join("\n");
        assert!(body.contains("\n  Nook"), "{body}");
        assert!(body.contains("\n    Scratch"), "{body}");
        assert!(body.contains("\n      Deep"), "{body}");
        assert!(body.contains("1 note"), "{body}");
        assert!(
            body.contains("(root)"),
            "a note at the root is still accounted for: {body}"
        );
    }

    /// A folder whose parent is not in the list is shown at the root rather
    /// than vanishing — the UI's `buildTree` rule, kept here so the two
    /// renderings agree about a tree the server can hand back mid-move.
    #[test]
    fn an_orphan_folder_is_shown_at_the_root() {
        let folders = vec![folder("f-orphan", "Adrift", Some("f-gone"))];
        assert!(render_tree(&folders, &[]).join("\n").contains("Adrift"));
    }

    // ── The two loops, against a notebook that races back ────────────────────

    /// A notebook in memory: enough of one to drive `ensure_folder` and
    /// `append_to` through the sequences that matter — a folder appearing under
    /// us, and a body being rewritten between our write and our read-back.
    #[derive(Default)]
    struct Fake {
        folders: RefCell<Vec<Value>>,
        body: RefCell<String>,
        sealed: Cell<bool>,
        creates: Cell<usize>,
        writes: Cell<usize>,
        reads: Cell<usize>,
        /// Seal the note once we have served this many reads; 0 = never.
        seals_after_read: Cell<usize>,
        /// Names whose first create is refused, standing in for MAIN-574's 409
        /// — and taken by the racing winner at the same moment.
        loses_race: RefCell<Vec<String>>,
        /// `(read number, body)` — another writer lands that body once we have
        /// served that many reads. Keyed on the READ rather than on our write,
        /// because the ordering AC-4 is about is the one where their edit
        /// arrives BEFORE ours: between the compose and the re-read, which is
        /// exactly where the check has to catch it.
        lands_after_read: RefCell<Vec<(usize, String)>>,
    }

    impl Fake {
        fn with_folders(rows: Vec<Value>) -> Self {
            Self {
                folders: RefCell::new(rows),
                ..Self::default()
            }
        }
    }

    impl NotebookApi for Fake {
        async fn folders(&self) -> Result<Vec<Value>> {
            Ok(self.folders.borrow().clone())
        }

        async fn create_folder(&self, name: &str, parent: Option<&str>) -> Result<Value> {
            let id = format!("f-{}", self.creates.get());
            self.creates.set(self.creates.get() + 1);
            let row = folder(&id, name, parent);
            let mut lost = self.loses_race.borrow_mut();
            if let Some(at) = lost.iter().position(|n| n == name) {
                lost.remove(at);
                // The winner's row is there by the time we look again.
                self.folders.borrow_mut().push(row);
                bail!("409 {FOLDERS}: a folder named \"{name}\" is already here");
            }
            self.folders.borrow_mut().push(row.clone());
            Ok(row)
        }

        async fn note(&self, id: &str) -> Result<Value> {
            let served = json!({
                "id": id,
                "content_md": self.body.borrow().clone(),
                "sealed": self.sealed.get(),
            });
            self.reads.set(self.reads.get() + 1);
            for (after, theirs) in self.lands_after_read.borrow().iter() {
                if *after == self.reads.get() {
                    *self.body.borrow_mut() = theirs.clone();
                }
            }
            if self.seals_after_read.get() == self.reads.get() {
                self.sealed.set(true);
            }
            Ok(served)
        }

        async fn write_body(&self, id: &str, body: &str) -> Result<(u16, Value)> {
            self.writes.set(self.writes.get() + 1);
            *self.body.borrow_mut() = body.to_string();
            Ok((
                200,
                json!({ "id": id, "content_md": body, "sealed": false }),
            ))
        }
    }

    /// AC-3: a nested path creates every missing level, and the same command
    /// again creates nothing and still succeeds.
    #[tokio::test]
    async fn mkdir_p_creates_every_level_then_reuses_them() {
        let api = Fake::default();
        let leaf = ensure_folder(&api, "Nook/Scratch/Deep").await.unwrap();
        assert_eq!(api.creates.get(), 3, "one create per missing level");
        assert_eq!(api.folders.borrow().len(), 3);

        let again = ensure_folder(&api, "Nook/Scratch/Deep").await.unwrap();
        assert_eq!(again, leaf, "the same folder comes back");
        assert_eq!(api.creates.get(), 3, "and nothing new was created");

        // A path branching off an existing one only creates its own tail.
        ensure_folder(&api, "Nook/Scratch/Shallow").await.unwrap();
        assert_eq!(api.creates.get(), 4);
    }

    /// AC-3: the 409 MAIN-574 added when two runs race on the same new folder
    /// is absorbed — the loser adopts the winner's folder instead of failing.
    #[tokio::test]
    async fn a_folder_taken_by_a_racing_run_is_adopted_not_reported() {
        let api = Fake::default();
        api.loses_race.borrow_mut().push("Deep".to_string());
        let leaf = ensure_folder(&api, "Nook/Scratch/Deep").await.unwrap();
        let folders = api.folders.borrow();
        let winner = folders
            .iter()
            .find(|f| f["name"] == "Deep")
            .expect("the winner's folder is there");
        assert_eq!(leaf.as_deref(), winner["id"].as_str());
    }

    /// A create that fails for a reason that is NOT somebody else getting there
    /// first still fails, and says which folder.
    #[tokio::test]
    async fn a_create_that_really_failed_is_still_an_error() {
        struct Broken;
        impl NotebookApi for Broken {
            async fn folders(&self) -> Result<Vec<Value>> {
                Ok(vec![])
            }
            async fn create_folder(&self, _: &str, _: Option<&str>) -> Result<Value> {
                bail!("500 the notebook is on fire")
            }
            async fn note(&self, _: &str) -> Result<Value> {
                unreachable!()
            }
            async fn write_body(&self, _: &str, _: &str) -> Result<(u16, Value)> {
                unreachable!()
            }
        }
        let e = ensure_folder(&Broken, "Nook/Deep")
            .await
            .expect_err("it fails")
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(": ");
        assert!(e.contains("\"Nook\""), "{e}");
        assert!(e.contains("on fire"), "{e}");
    }

    /// AC-4: an edit landing between our read and our write ABORTS the append,
    /// leaving the body exactly as the other writer left it. The old contract
    /// retried and merged here; this one refuses to write at all.
    #[tokio::test]
    async fn an_append_racing_another_edit_writes_nothing() {
        let api = Fake::default();
        *api.body.borrow_mut() = "hello".to_string();
        // Their edit lands once we have read once — after the compose, before
        // the re-read that guards the write.
        api.lands_after_read
            .borrow_mut()
            .push((1, "hello\n\nfrom the web UI".to_string()));

        let e = append_to(&api, "n-1", "Nook/test", "second block\n")
            .await
            .expect_err("it aborts")
            .to_string();
        assert!(
            e.contains("changed while this append was being prepared"),
            "{e}"
        );
        assert!(e.contains("NOTHING was written"), "it says so plainly: {e}");
        assert_eq!(api.writes.get(), 0, "and it means it");
        assert_eq!(
            *api.body.borrow(),
            "hello\n\nfrom the web UI",
            "their edit is untouched — not merged, not overwritten"
        );
    }

    #[tokio::test]
    async fn an_uncontested_append_writes_once() {
        let api = Fake::default();
        *api.body.borrow_mut() = "hello".to_string();
        let after = append_to(&api, "n-1", "Nook/test", "second block\n")
            .await
            .unwrap();
        assert_eq!(api.writes.get(), 1);
        assert_eq!(*api.body.borrow(), "hello\n\nsecond block");
        assert_eq!(
            after["content_md"].as_str().unwrap(),
            "hello\n\nsecond block",
            "the PATCH's own answer is what --json prints — no extra read"
        );
    }

    /// An append to an EMPTY note is not a false positive for drift: the body
    /// it composes from is `""` both times, and nothing moved.
    #[tokio::test]
    async fn an_empty_note_appends_without_reading_drift_into_it() {
        let api = Fake::default();
        let after = append_to(&api, "n-1", "Nook/blank", "first\n")
            .await
            .unwrap();
        assert_eq!(after["content_md"].as_str().unwrap(), "first");
        assert_eq!(api.writes.get(), 1);
    }

    /// Sealed between the two reads. Named as sealing rather than as drift —
    /// which matters precisely because a sealed body reads as `""`, so on an
    /// empty note the drift check alone would have seen nothing wrong and
    /// written server-encrypted plaintext over the seal.
    #[tokio::test]
    async fn a_note_sealed_mid_append_is_named_and_not_written_to() {
        let api = Fake::default();
        api.seals_after_read.set(1);
        let e = append_to(&api, "n-1", "Nook/blank", "first\n")
            .await
            .expect_err("it refuses")
            .to_string();
        assert!(e.contains("sealed"), "{e}");
        assert!(e.contains("web UI"), "{e}");
        assert_eq!(api.writes.get(), 0);
    }

    /// AC-7: a sealed note is refused BEFORE anything is written.
    #[tokio::test]
    async fn an_append_to_a_sealed_note_writes_nothing() {
        let api = Fake::default();
        api.sealed.set(true);
        let e = append_to(&api, "n-1", "Nook/private", "mine")
            .await
            .expect_err("it refuses")
            .to_string();
        assert!(e.contains("sealed"), "{e}");
        assert_eq!(api.writes.get(), 0, "and nothing was written");
    }

    /// `with_folders` keeps the id path honest: a folder addressed by id is
    /// checked against the notebook rather than trusted.
    #[tokio::test]
    async fn a_folder_id_is_checked_before_it_is_used() {
        let id = "0198f2c0-0000-7000-8000-000000000000";
        let api = Fake::with_folders(vec![folder(id, "ById", None)]);
        assert_eq!(ensure_folder(&api, id).await.unwrap().as_deref(), Some(id));
        assert_eq!(api.creates.get(), 0);

        let api = Fake::default();
        assert!(
            ensure_folder(&api, id).await.is_err(),
            "an unknown id fails"
        );
    }
}
