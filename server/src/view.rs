//! Revision diffs.
//!
//! Each side of a diff is a read-only snapshot file under
//! `~/Library/Caches/Revs/repos/<repo>/<folder>/`, mirroring the repository layout:
//! the right side is `<label> <name>` (label: short commit id, `Working`), the left side
//! `.<name>/<label>`. Zed shows two of them with its side-by-side diff
//! (`zed --diff left right:line:column`), so the tab reads
//! `1f3c2e9 ↔ 8a7b6c5 Invoice.php`. The left side needs no extension: Zed
//! highlights both sides with the right side's language (and skips parsing the left file).
//! Snapshots never change once written and are shared between diffs; one folder per source
//! folder keeps Zed's per-folder setup work to one run.
//!
//! A hidden `.<right file>.json` next to the right side records what the diff compares,
//! which diff it was reached from and where the step put the cursor, so whichever
//! Revs process Zed hands the buffer to knows what it is.

use crate::git::{Blobs, Repo};
use crate::history::Focus;
use serde_json::{Value, json};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

/// Revision id for the working tree contents.
pub const WORKING: &str = "WORKING";
/// Revision id for "the file doesn't exist here".
pub const EMPTY: &str = "EMPTY";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Side {
    pub rev: String,
    /// Path of the file at `rev`, relative to the repository root.
    pub path: String,
    pub author: String,
    pub mail: String,
    pub time: i64,
    pub summary: String,
}

impl Side {
    pub fn commit(sha: &str, path: &str) -> Side {
        Side {
            rev: sha.to_string(),
            path: path.to_string(),
            ..Side::default()
        }
    }

    pub fn working(path: &str) -> Side {
        Side {
            rev: WORKING.into(),
            path: path.to_string(),
            summary: "Uncommitted changes".into(),
            ..Side::default()
        }
    }

    pub fn empty(path: &str) -> Side {
        Side {
            rev: EMPTY.into(),
            path: path.to_string(),
            ..Side::default()
        }
    }

    pub fn is_commit(&self) -> bool {
        self.rev != WORKING && self.rev != EMPTY
    }

    /// Fills author/summary for a commit side when unknown.
    pub fn describe(&mut self, repo: &Repo) {
        if !self.is_commit() || !self.summary.is_empty() {
            return;
        }
        if let Some((author, mail, time, summary)) = repo.describe(&self.rev) {
            (self.author, self.mail, self.time, self.summary) = (author, mail, time, summary);
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "rev": self.rev, "path": self.path, "author": self.author,
            "mail": self.mail, "time": self.time, "summary": self.summary,
        })
    }

    fn from_json(value: &Value) -> Option<Side> {
        let text = |key: &str| value[key].as_str().unwrap_or_default().to_string();
        Some(Side {
            rev: value["rev"].as_str()?.to_string(),
            path: value["path"].as_str()?.to_string(),
            author: text("author"),
            mail: text("mail"),
            time: value["time"].as_i64().unwrap_or(0),
            summary: text("summary"),
        })
    }
}

fn focus_to_json(focus: Focus) -> Value {
    match focus {
        Focus::Right(line) => json!({ "side": "right", "line": line }),
        Focus::Left(line) => json!({ "side": "left", "line": line }),
    }
}

fn focus_from_json(value: &Value) -> Option<Focus> {
    let line = value["line"].as_u64()? as u32;
    match value["side"].as_str()? {
        "left" => Some(Focus::Left(line)),
        _ => Some(Focus::Right(line)),
    }
}

/// The diff a step came from, so the opposite step can return to it exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Back {
    pub left: Side,
    pub right: Side,
    /// Cursor row (right side) and the line it was about.
    pub line: u32,
    pub focus: Focus,
    /// The step that led here ("previous" or "next").
    pub command: String,
}

impl Back {
    fn to_json(&self) -> Value {
        json!({
            "left": self.left.to_json(), "right": self.right.to_json(), "line": self.line,
            "focus": focus_to_json(self.focus), "command": self.command,
        })
    }

    fn from_json(value: &Value) -> Option<Back> {
        Some(Back {
            left: Side::from_json(&value["left"])?,
            right: Side::from_json(&value["right"])?,
            line: value["line"].as_u64()? as u32,
            focus: focus_from_json(&value["focus"])?,
            command: value["command"].as_str()?.to_string(),
        })
    }
}

/// Where a step put the cursor in the diff it opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Landing {
    pub line: u32,
    pub character: u32,
    pub focus: Focus,
    /// When (Unix seconds); a landing only applies to a buffer opened right after.
    pub at: i64,
}

impl Landing {
    fn to_json(self) -> Value {
        json!({ "line": self.line, "character": self.character, "focus": focus_to_json(self.focus), "at": self.at })
    }

    fn from_json(value: &Value) -> Option<Landing> {
        Some(Landing {
            line: value["line"].as_u64()? as u32,
            character: value["character"].as_u64()? as u32,
            focus: focus_from_json(&value["focus"])?,
            at: value["at"].as_i64()?,
        })
    }
}

/// What a diff compares; stored beside its right-side file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pair {
    /// Project root opened in Zed; the Zed CLI runs there so the diff lands in that window.
    pub root: PathBuf,
    /// Repository root.
    pub repo: PathBuf,
    /// The file's current path relative to the repository root.
    pub file: String,
    pub left: Side,
    pub right: Side,
    pub left_path: PathBuf,
    pub right_path: PathBuf,
    pub back: Option<Back>,
    pub landing: Option<Landing>,
}

impl Pair {
    fn to_json(&self) -> Value {
        json!({
            "root": self.root, "repo": self.repo, "file": self.file,
            "left": self.left.to_json(), "right": self.right.to_json(),
            "leftPath": self.left_path, "rightPath": self.right_path,
            "back": self.back.as_ref().map(Back::to_json),
            "landing": self.landing.map(Landing::to_json),
        })
    }

    fn from_json(value: &Value) -> Option<Pair> {
        Some(Pair {
            root: PathBuf::from(value["root"].as_str()?),
            repo: PathBuf::from(value["repo"].as_str()?),
            file: value["file"].as_str()?.to_string(),
            left: Side::from_json(&value["left"])?,
            right: Side::from_json(&value["right"])?,
            left_path: PathBuf::from(value["leftPath"].as_str()?),
            right_path: PathBuf::from(value["rightPath"].as_str()?),
            back: Back::from_json(&value["back"]),
            landing: Landing::from_json(&value["landing"]),
        })
    }

    pub fn working_path(&self) -> PathBuf {
        self.repo.join(&self.file)
    }

    /// The pair whose right side is the snapshot at `path`, if `path` is one.
    pub fn load(path: &Path) -> Option<Pair> {
        Pair::read(&within_cache(path)?)
    }

    /// The record beside a right-side snapshot.
    fn read(right: &Path) -> Option<Pair> {
        let value: Value = serde_json::from_slice(&fs::read(sidecar(right)).ok()?).ok()?;
        Pair::from_json(&value)
    }

    /// The snapshot files this pair uses (kept by cleanup while open).
    pub fn files(&self) -> [PathBuf; 3] {
        [
            self.left_path.clone(),
            self.right_path.clone(),
            sidecar(&self.right_path),
        ]
    }
}

pub fn cache_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(home).join("Library/Caches/Revs")
}

/// Where repository snapshots live (apart from Revs's own files).
fn repos_dir() -> PathBuf {
    cache_dir().join("repos")
}

/// Persistent settings shared by every Revs process (e.g. the line blame toggle).
pub fn support_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(home).join("Library/Application Support/Revs")
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Unique within this machine for this process's lifetime.
pub fn unique_id() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

/// `path` as seen inside the snapshot folders (paths may arrive resolved,
/// `/private/var/…`, or not).
fn within_cache(path: &Path) -> Option<PathBuf> {
    let repos = repos_dir();
    if path.starts_with(&repos) {
        return Some(path.to_path_buf());
    }
    let resolved = fs::canonicalize(&repos).ok()?;
    let relative = path.strip_prefix(&resolved).ok()?;
    Some(repos.join(relative))
}

/// Whether `path` is a snapshot Revs wrote.
pub fn is_snapshot(path: &Path) -> bool {
    within_cache(path).is_some()
}

/// When the record beside a right-side snapshot was last written.
pub fn record_modified(right: &Path) -> Option<SystemTime> {
    fs::metadata(sidecar(right)).and_then(|meta| meta.modified()).ok()
}

fn sidecar(right: &Path) -> PathBuf {
    let name = right.file_name().map(|name| name.to_string_lossy()).unwrap_or_default();
    right.with_file_name(format!(".{name}.json"))
}

/// Contents of one side. `working` overrides the on-disk working file (unsaved buffer).
pub fn contents(blobs: &Blobs, side: &Side, working: Option<&[u8]>) -> Rc<[u8]> {
    match side.rev.as_str() {
        EMPTY => Rc::from(Vec::new()),
        WORKING => match working {
            Some(text) => Rc::from(text),
            None => Rc::from(fs::read(blobs.repo().root.join(&side.path)).unwrap_or_default()),
        },
        rev => blobs.get(rev, &side.path),
    }
}

/// A file or folder name made safe for paths Zed parses: no `/`, no `:` (Zed reads
/// `name:row`), no control characters, and not empty or dot-only.
fn safe_component(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|ch| match ch {
            '/' => '∕',
            ':' => '꞉',
            ch if ch.is_control() => ' ',
            ch => ch,
        })
        .collect();
    if out.trim_matches('.').is_empty() {
        out = format!("_{out}");
    }
    out
}

/// The cache folder for a repository: its folder name, made unique across repositories
/// with the same name (case-insensitively, as the disk compares them; recorded in
/// `repos.json`).
fn repo_folder(repo_root: &Path) -> io::Result<PathBuf> {
    let cache = cache_dir();
    fs::create_dir_all(repos_dir())?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(cache.join(".repos.lock"))?;
    use std::os::fd::AsRawFd as _;
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    let index_path = cache.join("repos.json");
    let mut index: serde_json::Map<String, Value> = fs::read(&index_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let key = repo_root.to_string_lossy().into_owned();
    let name = match index.get(&key).and_then(Value::as_str) {
        Some(name) => name.to_string(),
        None => {
            let base = safe_component(
                &repo_root
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "repository".into()),
            );
            let known: Vec<&str> = index.values().filter_map(Value::as_str).collect();
            // The disk decides what counts as the same name (case, Unicode forms).
            let name = free_name(&base, |candidate| {
                name_known(&known, candidate) || repos_dir().join(candidate).exists()
            });
            index.insert(key, Value::String(name.clone()));
            let tmp = cache.join(format!(".repos.json.{}", unique_id()));
            fs::write(&tmp, serde_json::to_vec_pretty(&index)?)?;
            fs::rename(&tmp, &index_path)?;
            name
        }
    };
    drop(lock);
    Ok(repos_dir().join(name))
}

/// `base`, or `base 2`, `base 3`, … — the first one not `taken`.
fn free_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    let mut name = base.to_string();
    let mut number = 2;
    while taken(&name) {
        name = format!("{base} {number}");
        number += 1;
    }
    name
}

/// Whether a repository folder name is in use, ignoring case as macOS disks do.
fn name_known(known: &[&str], candidate: &str) -> bool {
    known.iter().any(|name| name.to_lowercase() == candidate.to_lowercase())
}

fn write_read_only(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // Unique per process and call, so concurrent writers never share a temp file.
    let tmp = path.with_file_name(format!(".revs-{}.tmp", unique_id()));
    fs::write(&tmp, bytes)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o444))?;
    fs::rename(&tmp, path)
}

/// Marks a snapshot as in use (cleanup goes by modification time).
fn touch(path: &Path) {
    if let Ok(file) = fs::File::open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

/// Labels for a side, shortest first: short commit ids (longer ones only on a clash),
/// or "Working", "Working 2", … for working-tree versions.
fn labels(side: &Side) -> Vec<String> {
    match side.rev.as_str() {
        WORKING => (1..=200)
            .map(|n| if n == 1 { "Working".to_string() } else { format!("Working {n}") })
            .collect(),
        EMPTY => vec!["∅".into()],
        sha => [7, 10, 14, 40]
            .iter()
            .filter_map(|&length| sha.get(..length).map(str::to_string))
            .collect(),
    }
}

/// Writes one side's snapshot (unless an identical one exists) and returns its path. A
/// snapshot a diff may be showing is never rewritten: a different version takes the next
/// label. Only working-tree snapshots that no open diff shows and that weren't just
/// written get replaced (so they don't pile up). For a working-tree right side, `left`
/// must also match what the existing snapshot was compared with.
fn snapshot(folder: &Path, side: &Side, bytes: &[u8], is_left: bool, left: Option<&Side>) -> io::Result<PathBuf> {
    let relative = Path::new(&side.path);
    let mut dir = folder.to_path_buf();
    if let Some(parent) = relative.parent() {
        for part in parent.components() {
            dir.push(safe_component(&part.as_os_str().to_string_lossy()));
        }
    }
    let name = safe_component(
        &relative
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into()),
    );
    // Zed picks the language from the name's extension; names that are recognised as a
    // whole (Dockerfile, .gitignore) keep their name inside a label folder instead.
    let whole_name = !name.trim_start_matches('.').contains('.');
    for label in labels(side) {
        let path = if is_left {
            dir.join(format!(".{name}")).join(&label)
        } else if whole_name {
            dir.join(&label).join(&name)
        } else {
            dir.join(format!("{label} {name}"))
        };
        match fs::read(&path) {
            Ok(existing) if existing == bytes => {
                let same_pair = match (side.rev == WORKING, left) {
                    (true, Some(left)) => Pair::read(&path).is_none_or(|pair| pair.left.rev == left.rev),
                    _ => true,
                };
                if same_pair {
                    touch(&path);
                    return Ok(path);
                }
            }
            Ok(_) if side.rev == WORKING && unused(&path) => {
                write_read_only(&path, bytes)?;
                return Ok(path);
            }
            Ok(_) => {}
            Err(_) => {
                write_read_only(&path, bytes)?;
                return Ok(path);
            }
        }
    }
    Err(io::Error::other("couldn't name the snapshot"))
}

/// Whether no diff can be showing a snapshot: no running Revs has it open, and it is
/// older than a minute (a diff opening right now isn't registered yet).
fn unused(path: &Path) -> bool {
    let settled = fs::metadata(path)
        .and_then(|meta| meta.modified())
        .is_ok_and(|modified| modified < SystemTime::now() - Duration::from_secs(60));
    settled && !leased().iter().any(|file| file == path)
}

/// Writes both sides and the pair record; returns the pair (with the snapshot paths).
#[allow(clippy::too_many_arguments)]
pub fn prepare(
    blobs: &Blobs,
    root: &Path,
    file: &str,
    left: &Side,
    right: &Side,
    working: Option<&[u8]>,
    back: Option<Back>,
    landing: Option<Landing>,
) -> io::Result<Pair> {
    let folder = repo_folder(&blobs.repo().root)?;
    let left_path = snapshot(&folder, left, &contents(blobs, left, working), true, None)?;
    let right_path = snapshot(&folder, right, &contents(blobs, right, working), false, Some(left))?;
    let pair = Pair {
        root: root.to_path_buf(),
        repo: blobs.repo().root.clone(),
        file: file.to_string(),
        left: left.clone(),
        right: right.clone(),
        left_path,
        right_path,
        back,
        landing,
    };
    let record = sidecar(&pair.right_path);
    let tmp = record.with_file_name(format!(".revs-{}.tmp", unique_id()));
    fs::write(&tmp, serde_json::to_vec_pretty(&pair.to_json())?)?;
    fs::rename(tmp, record)?;
    Ok(pair)
}

/// The Zed CLI of the Zed that started this process (each language server keeps its own,
/// so two Zed apps never open diffs in each other).
static PARENT_CLI: OnceLock<PathBuf> = OnceLock::new();

/// Where the Zed CLI lives: the parent Zed's (language server), else the one the language
/// server last recorded (URL handler, command line).
pub fn zed_cli() -> PathBuf {
    if let Ok(path) = std::env::var("REVS_ZED_CLI") {
        return PathBuf::from(path);
    }
    if let Some(cli) = PARENT_CLI.get() {
        return cli.clone();
    }
    if let Ok(recorded) = fs::read_to_string(cache_dir().join("zed-cli")) {
        let recorded = PathBuf::from(recorded.trim());
        if recorded.exists() {
            return recorded;
        }
    }
    PathBuf::from("/Applications/Zed.app/Contents/MacOS/cli")
}

/// The Zed executable that started this process, if it is one.
fn parent_zed() -> Option<(i32, PathBuf)> {
    let parent = unsafe { libc::getppid() };
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length = unsafe { libc::proc_pidpath(parent, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if length <= 0 {
        return None;
    }
    Some((
        parent,
        PathBuf::from(String::from_utf8_lossy(&buffer[..length as usize]).as_ref()),
    ))
}

/// The CLI of the Zed that started this language server, if known.
pub fn parent_cli() -> Option<PathBuf> {
    PARENT_CLI.get().cloned()
}

/// Uses `cli` (from a hover card link: the Zed whose card was clicked) when it still is a
/// Zed CLI.
pub fn use_cli(cli: PathBuf) {
    if cli.ends_with("Contents/MacOS/cli") && cli.is_file() {
        let _ = PARENT_CLI.set(cli);
    }
}

pub fn record_zed_cli_from_parent() {
    let Some((_, executable)) = parent_zed() else {
        return;
    };
    let Some(macos_dir) = executable.parent() else {
        return;
    };
    let cli = macos_dir.join("cli");
    if macos_dir.ends_with("Contents/MacOS") && cli.exists() {
        let _ = PARENT_CLI.set(cli.clone());
        let _ = fs::create_dir_all(cache_dir());
        let _ = fs::write(cache_dir().join("zed-cli"), cli.to_string_lossy().as_bytes());
    }
}

/// Writes both sides and opens them in Zed's diff with the right side's cursor on 0-based
/// `line`, after its indentation. `focus` and `back` (the diff the step came from) are
/// recorded with the pair; `single_file` is passed to Zed too when the project is a single
/// file (so the diff lands in its window). Returns the pair and the cursor's UTF-16
/// column.
#[allow(clippy::too_many_arguments)]
pub fn show(
    blobs: &Blobs,
    root: &Path,
    file: &str,
    left: &Side,
    right: &Side,
    line: u32,
    focus: Focus,
    working: Option<&[u8]>,
    back: Option<Back>,
    single_file: Option<&Path>,
) -> io::Result<(Pair, u32)> {
    let right_text = contents(blobs, right, working);
    let column = indentation(&right_text, line);
    let landing = Landing {
        line,
        character: column,
        focus,
        at: now(),
    };
    let pair = prepare(blobs, root, file, left, right, working, back, Some(landing))?;
    open(&pair, line, column, single_file)?;
    Ok((pair, column))
}

/// UTF-16 column of the first non-blank character of 0-based `line`. A diff opens with
/// the cursor there rather than at column 0, which the language server reads as a click
/// on a removed (left-only) line.
pub fn indentation(text: &[u8], line: u32) -> u32 {
    let Some(content) = text.split(|&byte| byte == b'\n').nth(line as usize) else {
        return 0;
    };
    String::from_utf8_lossy(content)
        .chars()
        .take_while(|ch| ch.is_whitespace() && *ch != '\r')
        .map(|ch| ch.len_utf16() as u32)
        .sum()
}

/// Opens the pair in Zed's diff with the right side's cursor on 0-based `line` and
/// `column`, and waits until Zed has the tab open. Opens are serialized so quick repeats
/// stay in order.
pub fn open(pair: &Pair, line: u32, column: u32, single_file: Option<&Path>) -> io::Result<()> {
    static OPENING: Mutex<()> = Mutex::new(());
    let _turn = OPENING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let right = format!("{}:{}:{}", pair.right_path.display(), line + 1, column + 1);
    let mut command = Command::new(zed_cli());
    command.current_dir(&pair.root).arg("-e");
    if let Some(file) = single_file {
        command.arg(file);
    }
    let status = command
        .arg("--diff")
        .arg(&pair.left_path)
        .arg(&right)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("the Zed CLI exited with {status}")))
    }
}

/// Makes macOS open `revs://` links (the line blame card's) with the app bundle this server
/// runs from, the first time a card offers one. (Builds from source are registered when
/// installed.)
pub fn register_link_handler() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| {
        let Some(bundle) = std::env::current_exe().ok().and_then(|exe| {
            exe.ancestors()
                .nth(3)
                .filter(|bundle| bundle.extension().is_some_and(|extension| extension == "app"))
                .map(Path::to_path_buf)
        }) else {
            return;
        };
        let lsregister = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";
        let _ = Command::new(lsregister)
            .arg("-f")
            .arg(&bundle)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

/// Opens a file at a 0-based line through the Zed CLI (used outside the language server).
pub fn open_file(root: &Path, path: &Path, line: u32) -> io::Result<()> {
    Command::new(zed_cli())
        .current_dir(root)
        .arg("-e")
        .arg(format!("{}:{}", path.display(), line + 1))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| ())
}

// ---- leases and cleanup -----------------------------------------------------------------

fn leases_dir() -> PathBuf {
    cache_dir().join(".leases")
}

/// Records the snapshot files this process's open diffs use, so another process's
/// cleanup leaves them alone.
pub fn write_lease(files: &[PathBuf]) {
    let path = leases_dir().join(format!("{}.json", std::process::id()));
    if files.is_empty() {
        let _ = fs::remove_file(path);
        return;
    }
    let _ = fs::create_dir_all(leases_dir());
    let tmp = path.with_extension(format!("tmp-{}", unique_id()));
    let list: Vec<String> = files.iter().map(|file| file.to_string_lossy().into_owned()).collect();
    if fs::write(&tmp, serde_json::to_vec(&list).unwrap_or_default()).is_ok() {
        let _ = fs::rename(tmp, path);
    }
}

pub fn remove_lease() {
    let _ = fs::remove_file(leases_dir().join(format!("{}.json", std::process::id())));
}

/// Files leased by running Revs processes (stale leases are removed).
fn leased() -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(leases_dir()) else {
        return files;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let pid: Option<i32> = path.file_stem().and_then(|stem| stem.to_str()).and_then(|stem| stem.parse().ok());
        let alive = pid.is_some_and(|pid| unsafe { libc::kill(pid, 0) } == 0);
        if !alive {
            let _ = fs::remove_file(&path);
            continue;
        }
        if let Ok(bytes) = fs::read(&path)
            && let Ok(list) = serde_json::from_slice::<Vec<String>>(&bytes)
        {
            files.extend(list.into_iter().map(PathBuf::from));
        }
    }
    files
}

/// When the Zed process that started us was launched (diff tabs don't outlive it).
fn zed_started() -> Option<SystemTime> {
    let (pid, executable) = parent_zed()?;
    if !executable.to_string_lossy().contains(".app/Contents/MacOS/") {
        return None;
    }
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let read = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size)
    };
    (read == size).then(|| {
        SystemTime::UNIX_EPOCH
            + Duration::from_secs(info.pbi_start_tvsec)
            + Duration::from_micros(info.pbi_start_tvusec)
    })
}

/// Deletes snapshots no diff can still show: untouched since Zed started (Zed doesn't
/// restore diff tabs) or for a week, except files another running Revs has open.
/// Also tidies Revs's own leftovers.
pub fn clean_old_snapshots() {
    let cache = cache_dir();
    // Earlier layouts.
    let _ = fs::remove_dir_all(cache.join("views"));
    let _ = fs::remove_dir_all(cache.join(".tmp"));
    let _ = fs::remove_file(cache.join("link-token"));
    // The protocol trace never grows past a few MB.
    let trace_log = cache.join("trace.log");
    if fs::metadata(&trace_log).is_ok_and(|meta| meta.len() > 8 << 20) {
        let _ = fs::remove_file(&trace_log);
    }
    clean_links();
    let week = SystemTime::now() - Duration::from_secs(7 * 24 * 3600);
    let cutoff = zed_started().map_or(week, |started| started.max(week));
    let keep = leased();
    clean_dir(&repos_dir(), cutoff, &keep);
    // Repository folders of the earlier layout, directly in the cache (any name but
    // Revs's own).
    for entry in fs::read_dir(&cache).into_iter().flatten().filter_map(Result::ok) {
        let own = matches!(entry.file_name().to_str(), Some("repos" | ".leases" | ".links" | ".dirty" | ".wants"));
        if !own && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            clean_dir(&entry.path(), cutoff, &keep);
            let _ = fs::remove_dir(entry.path());
        }
    }
}

fn clean_dir(dir: &Path, cutoff: SystemTime, keep: &[PathBuf]) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            clean_dir(&path, cutoff, keep);
            let _ = fs::remove_dir(&path); // only succeeds once empty
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if modified < cutoff && !keep.iter().any(|kept| kept == &path) {
            let _ = fs::remove_file(&path);
        }
    }
}

// ---- hover card links -------------------------------------------------------------------

fn links_dir() -> PathBuf {
    cache_dir().join(".links")
}

/// Stores what a hover card's link should open and returns a random id for the link.
/// Links only work on this Mac, for a day.
pub fn store_link(record: &Value) -> Option<String> {
    let mut bytes = [0u8; 16];
    if unsafe { libc::getentropy(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
        return None;
    }
    let id: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    fs::create_dir_all(links_dir()).ok()?;
    let _ = fs::set_permissions(links_dir(), fs::Permissions::from_mode(0o700));
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(links_dir().join(&id))
        .ok()?;
    file.write_all(&serde_json::to_vec(record).ok()?).ok()?;
    Some(id)
}

/// The record behind a link id, while the link is less than a day old.
pub fn load_link(id: &str) -> Option<Value> {
    if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let path = links_dir().join(id);
    let modified = fs::metadata(&path).and_then(|meta| meta.modified()).ok()?;
    if modified < SystemTime::now() - Duration::from_secs(24 * 3600) {
        return None;
    }
    serde_json::from_slice(&fs::read(&path).ok()?).ok()
}

fn clean_links() {
    let Ok(entries) = fs::read_dir(links_dir()) else {
        return;
    };
    let day = SystemTime::now() - Duration::from_secs(24 * 3600);
    for entry in entries.filter_map(Result::ok) {
        if entry.metadata().and_then(|meta| meta.modified()).is_ok_and(|modified| modified < day) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Whether the opt-in protocol trace is on (`touch ~/Library/Caches/Revs/trace`).
pub fn trace_enabled() -> bool {
    fs::metadata(cache_dir().join("trace")).is_ok_and(|meta| meta.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_are_safe() {
        assert_eq!(safe_component("a/b:c"), "a∕b꞉c");
        assert_eq!(safe_component(".."), "_..");
        assert_eq!(safe_component("tab\there"), "tab here");
        assert_eq!(safe_component("Invoice.php"), "Invoice.php");
    }

    #[test]
    fn sidecar_names() {
        assert_eq!(
            sidecar(Path::new("/c/repo/app/8a7b6c5 Invoice.php")),
            Path::new("/c/repo/app/.8a7b6c5 Invoice.php.json")
        );
    }

    #[test]
    fn snapshot_names() {
        let dir = std::env::temp_dir().join(format!("revs-test-{}", unique_id()));
        let commit = Side::commit("8a7b6c5d4e3f2a1b", "app/Invoice.php");
        let right = snapshot(&dir, &commit, b"x\n", false, None).unwrap();
        assert_eq!(right, dir.join("app/8a7b6c5 Invoice.php"));
        let left = snapshot(&dir, &commit, b"x\n", true, None).unwrap();
        assert_eq!(left, dir.join("app/.Invoice.php/8a7b6c5"));
        // Same short id, different contents (another commit): a longer label.
        let clash = Side::commit("8a7b6c5b00000000", "app/Invoice.php");
        assert_eq!(snapshot(&dir, &clash, b"y\n", false, None).unwrap(), dir.join("app/8a7b6c5b00 Invoice.php"));
        // Whole-name files keep their name.
        let docker = Side::commit("8a7b6c5d4e3f2a1b", "Dockerfile");
        assert_eq!(snapshot(&dir, &docker, b"FROM x\n", false, None).unwrap(), dir.join("8a7b6c5/Dockerfile"));
        // Working versions are never rewritten: a different version takes the next label.
        let working = Side::working("app/Invoice.php");
        let head = Side::commit("1111111aaaa", "app/Invoice.php");
        let first = snapshot(&dir, &working, b"one\n", false, Some(&head)).unwrap();
        assert_eq!(first, dir.join("app/Working Invoice.php"));
        let second = snapshot(&dir, &working, b"two\n", false, Some(&head)).unwrap();
        assert_eq!(second, dir.join("app/Working 2 Invoice.php"));
        assert_eq!(fs::read(&first).unwrap(), b"one\n");
        assert_eq!(snapshot(&dir, &working, b"one\n", false, Some(&head)).unwrap(), first);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn repository_folder_names() {
        let known = ["Website", "api"];
        assert!(name_known(&known, "website"));
        assert!(!name_known(&known, "web"));
        assert_eq!(free_name("website", |name| name_known(&known, name)), "website 2");
        assert_eq!(free_name("Blog", |name| name_known(&known, name)), "Blog");
    }

    #[test]
    fn working_snapshots_follow_their_left_side() {
        let dir = std::env::temp_dir().join(format!("revs-test-{}", unique_id()));
        let working = Side::working("f.txt");
        let first_left = Side::commit("aaaaaaaaaa", "f.txt");
        let path = snapshot(&dir, &working, b"same\n", false, Some(&first_left)).unwrap();
        let record = json!({
            "root": "/r", "repo": "/r", "file": "f.txt",
            "left": first_left.to_json(), "right": working.to_json(),
            "leftPath": "/x", "rightPath": path,
        });
        fs::write(sidecar(&path), serde_json::to_vec(&record).unwrap()).unwrap();
        // The same text compared with another commit gets its own file (an open diff of
        // the first pair keeps reading its own).
        let other_left = Side::commit("bbbbbbbbbb", "f.txt");
        let other = snapshot(&dir, &working, b"same\n", false, Some(&other_left)).unwrap();
        assert_ne!(other, path);
        assert_eq!(snapshot(&dir, &working, b"same\n", false, Some(&first_left)).unwrap(), path);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_sha_format() {
        assert_eq!(crate::format::short_sha("8a7b6c5d4e3f2a1b"), "8a7b6c5");
    }
}
