//! The blame hover card, the `revs://` link handler and the command line.

use crate::diff;
use crate::format;
use crate::git::{self, Blobs, Repo};
use crate::history::{self, Focus, Outcome};
use crate::view::{self, Pair, Side};
use serde_json::json;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub enum LineTarget {
    Commit {
        sha: String,
        /// The file's path in `sha`.
        path: String,
        previous: Option<(String, String)>,
        /// 0-based line in `sha`'s version of the file.
        orig: u32,
        author: String,
        mail: String,
        time: i64,
        summary: String,
    },
    Uncommitted {
        line: u32,
        /// The line and the whole text as the buffer has them, which may differ from the
        /// saved file.
        text: String,
        buffer: Arc<str>,
    },
}

// ---- hover card (line blame annotation) ------------------------------------------------

pub struct Tooltip {
    pub key: String,
    repo: Arc<Repo>,
    root: PathBuf,
    single_file: Option<PathBuf>,
    rel: String,
    target: LineTarget,
}

impl Tooltip {
    pub fn for_line(
        repo: Arc<Repo>,
        root: PathBuf,
        single_file: Option<PathBuf>,
        rel: String,
        target: LineTarget,
    ) -> Tooltip {
        let key = match &target {
            LineTarget::Commit { sha, path, orig, .. } => format!("line:{sha}:{path}:{orig}"),
            LineTarget::Uncommitted { line, .. } => format!("uncommitted:{rel}:{line}"),
        };
        Tooltip {
            key,
            repo,
            root,
            single_file,
            rel,
            target,
        }
    }

    /// Whether the card can be reused for the same line later: commit cards can; an
    /// uncommitted line's card depends on the unsaved text (and costs no git to make).
    pub fn cacheable(&self) -> bool {
        matches!(self.target, LineTarget::Commit { .. })
    }

    pub fn build(&self) -> String {
        let changes = match self.changes_url() {
            Some(url) => format!("[Open Changes with Previous Revision]({url})"),
            None => String::new(),
        };
        let LineTarget::Commit {
            sha,
            path,
            previous,
            orig,
            author,
            mail,
            time,
            summary,
        } = &self.target
        else {
            return format!("**You** · Uncommitted changes\n\n{changes}");
        };
        let mut out = self.header(author, mail, *time);
        out.push_str("\n\n");
        out.push_str(&self.message(sha, summary));
        out.push_str("\n\n");
        out.push_str(&format!("`{}`", format::short_sha(sha)));
        if !changes.is_empty() {
            out.push_str(&format!(" · {changes}"));
        }
        if let Some(remote) = self.repo.remote() {
            out.push_str(&format!(
                " · [Open Commit on {}]({})",
                remote.name(),
                remote.commit_url(sha)
            ));
        }
        if let Some((parent, parent_path)) = previous
            && let Some(snippet) = self.snippet(parent, parent_path, sha, path, *orig)
        {
            out.push_str("\n\n```diff\n");
            out.push_str(&snippet);
            out.push_str("```");
        }
        out
    }

    fn header(&self, author: &str, mail: &str, time: i64) -> String {
        let who = if self.repo.is_you(author, mail) {
            "You".to_string()
        } else {
            format::escape_markdown(author)
        };
        format!(
            "**{who}**, {} ({})",
            format::relative_time(time, format::now()),
            format::absolute_time(time)
        )
    }

    fn message(&self, sha: &str, summary: &str) -> String {
        let remote = self.repo.remote();
        let message = self.repo.message(sha).unwrap_or_else(|| summary.to_string());
        let mut lines = message.lines();
        let subject = lines.next().unwrap_or(summary);
        let body: Vec<&str> = lines.skip_while(|line| line.trim().is_empty()).collect();
        let mut out = format::linkify_message(subject, remote);
        if !body.is_empty() {
            out.push_str("\n\n");
            let shown = body.len().min(15);
            let mut body_text = body[..shown]
                .iter()
                .map(|line| format::linkify_message(line, remote))
                .collect::<Vec<_>>()
                .join("  \n");
            if body.len() > shown {
                body_text.push_str("  \n…");
            }
            out.push_str(&body_text);
        }
        out
    }

    /// A `revs://` link for "Open Changes with Previous Revision": a random id for a
    /// record (in Revs's cache) of exactly what to open.
    fn changes_url(&self) -> Option<String> {
        let mut record = json!({
            "root": self.root,
            "repo": self.repo.root,
            "file": self.rel,
        });
        if let Some(single_file) = &self.single_file {
            record["singleFile"] = json!(single_file);
        }
        // The link opens in the Zed whose card was clicked, not whichever started Revs
        // last.
        if let Some(cli) = view::parent_cli() {
            record["cli"] = json!(cli);
        }
        match &self.target {
            LineTarget::Commit {
                sha,
                path,
                previous,
                orig,
                ..
            } => {
                record["sha"] = json!(sha);
                record["path"] = json!(path);
                if let Some((parent, parent_path)) = previous {
                    record["prev"] = json!(parent);
                    record["prevPath"] = json!(parent_path);
                }
                record["line"] = json!(orig);
            }
            LineTarget::Uncommitted { line, text, buffer } => {
                // Where the line is in the saved file: the link handler only has that. A
                // line typed since the last save has no place there yet.
                record["working"] = json!(true);
                let saved = self.repo.read_working(&self.rel).unwrap_or_default();
                let hunks = diff::hunks(&String::from_utf8_lossy(&saved), buffer);
                match diff::new_to_old(&hunks, *line) {
                    diff::NewToOld::Unchanged(saved_line) => {
                        record["line"] = json!(saved_line);
                        record["text"] = json!(text);
                    }
                    _ => record["unsaved"] = json!(true),
                }
            }
        }
        let id = view::store_link(&record)?;
        view::register_link_handler();
        Some(format!("revs://changes?id={id}"))
    }

    /// The hunk of the commit's diff that touched `orig` (0-based, new side).
    fn snippet(&self, parent: &str, parent_path: &str, sha: &str, path: &str, orig: u32) -> Option<String> {
        let diff = self
            .repo
            .diff_blobs(&format!("{parent}:{parent_path}"), &format!("{sha}:{path}"))?;
        let target = orig + 1;
        let mut lines: Vec<&str> = Vec::new();
        let mut inside = false;
        for line in diff.lines() {
            if line.starts_with("@@ ") {
                if inside {
                    break;
                }
                let hunk = git::parse_hunks(line).into_iter().next()?;
                inside = hunk.new_count > 0
                    && target >= hunk.new_start
                    && target < hunk.new_start + hunk.new_count;
                continue;
            }
            if inside && (line.starts_with('-') || line.starts_with('+')) {
                lines.push(line);
            }
        }
        if lines.is_empty() {
            return None;
        }
        const MAX_LINES: usize = 12;
        let mut out = String::new();
        for line in lines.iter().take(MAX_LINES) {
            out.push_str(&format::truncate(line, 160));
            out.push('\n');
        }
        if lines.len() > MAX_LINES {
            out.push_str(&format!("… {} more lines\n", lines.len() - MAX_LINES));
        }
        Some(out)
    }
}

// ---- opening outcomes outside the language server --------------------------------------

/// Opens the diff an outcome names, or returns the reason nothing opens.
fn carry_out(
    blobs: &Blobs,
    root: &Path,
    single_file: Option<&Path>,
    file: &str,
    outcome: Outcome,
) -> io::Result<Option<String>> {
    match outcome {
        Outcome::Open {
            left,
            mut right,
            line,
            focus,
            note,
        } => {
            right.describe(blobs.repo());
            view::show(blobs, root, file, &left, &right, line, focus, None, None, single_file)?;
            Ok(note)
        }
        Outcome::Stay(message) => Ok(Some(message)),
        // Without a window to ask in, show the first (newest) candidate.
        Outcome::Choose { mut choices, .. } => match choices.drain(..).next() {
            Some((_, outcome)) => carry_out(blobs, root, single_file, file, outcome),
            None => Ok(None),
        },
    }
}

// ---- URL handler -----------------------------------------------------------------------

/// Handles `revs://changes?id=…` (from the line blame hover card). The id names a
/// record Revs stored when it made the card; links work only on this Mac, for up to a day.
pub fn handle_url(url: &str) -> io::Result<()> {
    let rest = url
        .strip_prefix("revs://")
        .ok_or_else(|| io::Error::other("not a revs:// URL"))?;
    let (action, query) = rest.split_once('?').unwrap_or((rest, ""));
    if action.trim_end_matches('/') != "changes" {
        return Err(io::Error::other(format!("unknown action {action}")));
    }
    let id = format::parse_query(query)
        .into_iter()
        .find(|(name, _)| name == "id")
        .map(|(_, value)| value)
        .unwrap_or_default();
    let record = view::load_link(&id).ok_or_else(|| io::Error::other("this link has expired or wasn't made by Revs"))?;
    let text = |key: &str| record[key].as_str().map(str::to_string);
    let commit = |key: &str| -> io::Result<Option<String>> {
        match text(key) {
            Some(value) if git::is_commit_id(&value) => Ok(Some(value)),
            Some(_) => Err(io::Error::other(format!("invalid {key}"))),
            None => Ok(None),
        }
    };
    if let Some(cli) = text("cli") {
        view::use_cli(PathBuf::from(cli));
    }
    let repo_root = PathBuf::from(text("repo").ok_or_else(|| io::Error::other("missing repo"))?);
    let repo = Repo::discover(&repo_root).ok_or_else(|| io::Error::other("not a repository"))?;
    let blobs = Blobs::new(&repo);
    let root = text("root").map_or_else(|| repo.root.clone(), PathBuf::from);
    let single_file = text("singleFile").map(PathBuf::from);
    let file = text("file").ok_or_else(|| io::Error::other("missing file"))?;
    let line = record["line"].as_u64().unwrap_or(0) as u32;
    let outcome = if record["working"].as_bool() == Some(true) {
        if record["unsaved"].as_bool() == Some(true) {
            return Err(io::Error::other("this line isn't saved yet: save the file first"));
        }
        let saved = repo.read_working(&file)?;
        let wanted = text("text").unwrap_or_default();
        if history::lines_of(&saved).get(line as usize) != Some(&wanted.as_bytes()) {
            return Err(io::Error::other("the file changed since the card was made: hover the line again"));
        }
        history::previous_in_file(&blobs, &file, &saved, line, None)?
    } else {
        let sha = commit("sha")?.ok_or_else(|| io::Error::other("missing sha"))?;
        let path = text("path").unwrap_or_else(|| file.clone());
        let left = match commit("prev")? {
            Some(parent) => Side::commit(&parent, &text("prevPath").unwrap_or_else(|| path.clone())),
            None => Side::empty(&path),
        };
        Outcome::Open {
            left,
            right: Side::commit(&sha, &path),
            line,
            focus: Focus::Right(line),
            note: None,
        }
    };
    carry_out(&blobs, &root, single_file.as_deref(), &file, outcome).map(|_| ())
}

// ---- command line ----------------------------------------------------------------------

/// `revs <previous|next|open-file> <file> <row>` for scripts and key bindings.
pub fn command_line(command: &str, args: &[String]) -> i32 {
    let (Some(file), row) = (args.first(), args.get(1)) else {
        eprintln!("usage: revs {command} <file> <row>");
        return 2;
    };
    let path = PathBuf::from(file);
    let line = row
        .and_then(|row| row.parse::<u32>().ok())
        .map_or(0, |row| row.saturating_sub(1));
    match run_command(command, &path, line) {
        Ok(Some(message)) => {
            println!("{message}");
            0
        }
        Ok(None) => 0,
        Err(error) => {
            eprintln!("revs: {error}");
            1
        }
    }
}

fn run_command(command: &str, path: &Path, line: u32) -> io::Result<Option<String>> {
    if let Some(pair) = Pair::load(path) {
        let repo = Repo::discover(&pair.repo).ok_or_else(|| io::Error::other("not a repository"))?;
        let blobs = Blobs::new(&repo);
        return match command {
            "previous" | "changes" => {
                let right = fs::read(&pair.right_path)?;
                let outcome = history::previous_on_right(&blobs, &pair.left, &pair.right, &right, line)?;
                carry_out(&blobs, &pair.root, None, &pair.file, outcome)
            }
            "next" => {
                let outcome = history::next_from(&blobs, &pair.right, line, None)?;
                carry_out(&blobs, &pair.root, None, &pair.file, outcome)
            }
            "open-file" => {
                let right = fs::read(&pair.right_path)?;
                let working = fs::read(pair.working_path())?;
                let (target, note) = history::working_line(&blobs, &pair.right, &right, line, &working);
                view::open_file(&pair.root, &pair.working_path(), target)?;
                Ok(note)
            }
            _ => Err(io::Error::other(format!("unknown command {command}"))),
        };
    }
    let path = fs::canonicalize(path)?;
    let repo = Repo::discover(&path).ok_or_else(|| io::Error::other("not in a git repository"))?;
    let blobs = Blobs::new(&repo);
    let rel = repo
        .relative(&path)
        .ok_or_else(|| io::Error::other("file is outside the repository"))?;
    let root = project_root(&repo);
    match command {
        "previous" | "changes" => {
            let text = fs::read(&path)?;
            let outcome = history::previous_in_file(&blobs, &rel, &text, line, None)?;
            carry_out(&blobs, &root, None, &rel, outcome)
        }
        "next" => Ok(Some("This is the working file: there's nothing newer".into())),
        "open-file" => Ok(Some("This is already the working file".into())),
        _ => Err(io::Error::other(format!("unknown command {command}"))),
    }
}

/// The directory Zed has open: tasks run there, so prefer the current directory.
fn project_root(repo: &Repo) -> PathBuf {
    std::env::var_os("ZED_WORKTREE_ROOT")
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| repo.root.clone())
}
