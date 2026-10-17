//! The language server: commands that open a line's previous/next change in Zed's
//! side-by-side diff, plus an optional current-line blame annotation.
//!
//! Zed lists a server's commands in its LSP command selector (Cmd-Shift-P → "lsp command
//! selector: toggle"), so the commands carry readable names. They act on the line where
//! the cursor last was: Zed asks every server for code actions about 250 ms after the
//! cursor settles, and that request carries the position. In a diff only the right side
//! reaches language servers; a cursor on the left is mirrored onto the right side's
//! aligned row, which is enough to tell left from right except on removed lines (see
//! `classify`). After a step Revs knows exactly which line the new diff is about.

use crate::actions;
use crate::blame::{Blame, Commit, EDITED, LineBlame};
use crate::diff;
use crate::format;
use crate::git::{Blobs, Repo};
use crate::history::{self, Focus, Outcome};
use crate::rpc;
use crate::share;
use crate::text::{LineEdit, Text, shift_line};
use crate::view::{self, Back, Pair, Side};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime};

// Zed lists a server's commands sorted by name. Zero-width spaces after "Revs:" (none
// for the first, one more for each after it) keep the names as they read and put them in
// this order: Previous, Toggle Line Blame, Next, Open Working File, Open Commit.
pub const PREVIOUS: &str = "Revs: Previous Revision";
pub const TOGGLE_BLAME: &str = "Revs:\u{200B} Toggle Line Blame";
pub const NEXT: &str = "Revs:\u{200B}\u{200B} Next Revision";
pub const OPEN_FILE: &str = "Revs:\u{200B}\u{200B}\u{200B} Open Working File";
const OPEN_COMMIT_PREFIX: &str = "Revs:\u{200B}\u{200B}\u{200B}\u{200B} Open Commit on ";

/// Files above this many lines get a single-line blame first so the annotation shows up
/// before the full blame finishes.
const QUICK_BLAME_LINES: usize = 3000;

/// Generations are unique across documents, so a job started for a closed document can
/// never be mistaken for one of a reopened document with the same URI.
static GENERATIONS: AtomicU64 = AtomicU64::new(1);

fn next_generation() -> u64 {
    GENERATIONS.fetch_add(1, Ordering::Relaxed)
}

pub enum Event {
    Message(Value),
    Eof,
    Blame {
        uri: String,
        generation: u64,
        head: Option<String>,
        blame: Option<Blame>,
    },
    QuickBlame {
        uri: String,
        generation: u64,
        /// Document version the blamed text belongs to.
        version: u64,
        line: u32,
        blame: Option<Blame>,
    },
    Respond {
        id: Value,
        result: Value,
    },
    ShowDocument {
        uri: String,
        line: Option<u32>,
        character: u32,
        external: bool,
    },
    /// Plain text for a bottom-right card.
    ShowMessage(String),
    /// Markdown (already escaped where needed) for a bottom-right card.
    Card(String),
    /// A diff opened (`pair`, as recorded beside its right side) with the cursor on
    /// `line`/`character`, about `focus`.
    Opened {
        pair: Box<Pair>,
        line: u32,
        character: u32,
        focus: Focus,
    },
    /// Ask which of several readings to show.
    Choose(Box<Question>),
    Tooltip {
        key: String,
        markdown: Arc<str>,
    },
    /// Another Revs process asks for the unsaved text of a working file.
    Want {
        id: String,
        path: PathBuf,
    },
}

#[derive(Clone, PartialEq, Eq)]
struct Settings {
    /// Line blame unless toggled with the command (the toggle is remembered).
    annotation: bool,
    fast_cursor: bool,
    /// A card naming the commit after each step.
    step_card: bool,
    summary_length: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            annotation: false,
            fast_cursor: false,
            step_card: true,
            summary_length: 50,
        }
    }
}

impl Settings {
    fn apply(&mut self, value: &Value) {
        let flag = |key: &str, slot: &mut bool| {
            if let Some(flag) = value.get(key).and_then(Value::as_bool) {
                *slot = flag;
            }
        };
        flag("annotation", &mut self.annotation);
        flag("fastCursor", &mut self.fast_cursor);
        flag("stepCard", &mut self.step_card);
        if let Some(length) = value.get("summaryLength").and_then(Value::as_u64) {
            self.summary_length = (length as usize).clamp(10, 200);
        }
    }
}

/// The line blame toggle, shared by every Revs process through a small file.
struct Toggle {
    path: PathBuf,
    checked: Instant,
    modified: Option<SystemTime>,
    value: Option<bool>,
}

impl Toggle {
    fn new() -> Toggle {
        let mut toggle = Toggle {
            path: view::support_dir().join("state.json"),
            checked: Instant::now() - Duration::from_secs(60),
            modified: None,
            value: None,
        };
        toggle.reload();
        toggle
    }

    fn reload(&mut self) {
        self.checked = Instant::now();
        let modified = std::fs::metadata(&self.path).and_then(|meta| meta.modified()).ok();
        if modified == self.modified {
            return;
        }
        self.modified = modified;
        self.value = std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|state| state["lineBlame"].as_bool());
    }

    /// Picks up changes made by other Revs processes (checked at most every 500 ms).
    fn refresh(&mut self) -> bool {
        if self.checked.elapsed() < Duration::from_millis(500) {
            return false;
        }
        let before = self.value;
        self.reload();
        before != self.value
    }

    fn set(&mut self, on: bool) {
        self.value = Some(on);
        let _ = std::fs::create_dir_all(view::support_dir());
        let tmp = self.path.with_file_name(format!(".state-{}.json", view::unique_id()));
        let state = serde_json::to_vec(&json!({ "lineBlame": on })).unwrap_or_default();
        if view::write_new(&tmp, &state, 0o600).is_ok() && std::fs::rename(&tmp, &self.path).is_ok() {
            self.modified = std::fs::metadata(&self.path).and_then(|meta| meta.modified()).ok();
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

struct Working {
    repo: Arc<Repo>,
    rel: String,
    blame: Option<Blame>,
    /// HEAD at the last finished attempt.
    blame_head: Option<String>,
    /// A blame attempt finished (it may have failed, e.g. for an untracked file).
    attempted: bool,
    /// Needs a fresh blame (saved, or HEAD moved).
    stale: bool,
    pending: bool,
    /// Edits made while a blame runs, replayed onto its result.
    edits_during_blame: Vec<LineEdit>,
    /// Single-line result while a full blame of a large file runs.
    quick: Option<(u32, Blame)>,
    /// Line of the latest single-line blame (running or done).
    quick_line: Option<u32>,
    quick_in_flight: bool,
    /// The text (and its version) the running full blame works on, reused by
    /// single-line blames while the document hasn't changed.
    snapshot: Option<(u64, Arc<str>)>,
    /// The file uses CRLF on disk; Zed sends LF, git compares against the CRLF blob.
    crlf: bool,
}

enum Kind {
    Plain,
    Working(Box<Working>),
    /// The right side of a Revs diff.
    View(Box<Pair>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cursor {
    line: u32,
    /// UTF-16 column of the selection start.
    character: u32,
    /// Last line of a selection (`line` when nothing spans lines).
    last_line: u32,
    /// No text selected (a mirrored left-side cursor is always empty).
    empty: bool,
    /// Revs put it there by opening a diff (no user movement since).
    placed: bool,
}

impl Cursor {
    fn at(line: u32, character: u32) -> Cursor {
        Cursor {
            line,
            character,
            last_line: line,
            empty: true,
            placed: false,
        }
    }
}

struct Doc {
    text: Text,
    /// Bumped on every change.
    version: u64,
    echo: Option<Echo>,
    /// When the record of a diff (its sidecar) was last read.
    record_modified: Option<SystemTime>,
    /// Edited since it was opened or saved.
    dirty: bool,
    cursor: Option<Cursor>,
    /// What the diff Revs opened is about, while the cursor is still where it put it.
    focus: Option<Focus>,
    kind: Kind,
    /// Bumped on close and on each new blame; jobs carrying an older value are dropped.
    generation: Arc<AtomicU64>,
}

impl Doc {
    fn cursor_line(&self) -> Option<u32> {
        self.cursor.map(|cursor| cursor.line)
    }
}

type Jobs = Arc<(Mutex<VecDeque<Box<dyn FnOnce() + Send>>>, Condvar)>;

struct Pool {
    queue: Jobs,
}

impl Pool {
    fn new(workers: usize) -> Pool {
        let queue: Jobs = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
        for _ in 0..workers {
            let queue = queue.clone();
            std::thread::spawn(move || {
                loop {
                    let job = {
                        let (lock, ready) = &*queue;
                        let mut jobs = lock.lock().unwrap();
                        loop {
                            if let Some(job) = jobs.pop_front() {
                                break job;
                            }
                            jobs = ready.wait(jobs).unwrap();
                        }
                    };
                    // A panicking job must not take the worker down with it.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                }
            });
        }
        Pool { queue }
    }

    fn spawn(&self, job: impl FnOnce() + Send + 'static) {
        let (lock, ready) = &*self.queue;
        lock.lock().unwrap().push_back(Box::new(job));
        ready.notify_one();
    }
}

#[derive(Default)]
struct Refresh {
    in_flight: Option<i64>,
    queued: bool,
}

/// The latest edit of a document. Zed then has every editor showing the file report its
/// cursor (all at once, 250 ms later): the one that typed, other panes on the file, and
/// tabs the user isn't in (a file reloaded from disk).
struct Echo {
    /// Lines the edit spans once made.
    first: u32,
    last: u32,
    at: Instant,
    /// The edit began at the cursor Revs last had for this document: someone is typing
    /// or pasting there (after a tab switch, Zed doesn't report those).
    at_cursor: bool,
    /// When the report from the editor that typed arrived.
    settled: Option<Instant>,
}

/// How long after an edit cursor reports may be echoes of it.
const ECHO_WINDOW: Duration = Duration::from_millis(700);
/// Reports closer together than this come from one refresh, not from clicks.
const BURST: Duration = Duration::from_millis(100);

/// What a report right after an edit is.
enum Echoed {
    /// No recent edit.
    No,
    /// The editor the user is typing in.
    User,
    /// A tab the user isn't in: remember its cursor, don't switch to it.
    Background,
    /// Another pane on the file, reporting together with the typing one.
    OtherPane,
}

/// The latest accepted cursor report, to undo its switch if it turns out to be the start
/// of a refresh burst.
struct LastReport {
    uri: String,
    at: Instant,
    /// `active` and `recent` before it, if it switched documents.
    before: Option<(Option<String>, VecDeque<String>)>,
}

/// A question about which reading to show, with the step to finish once answered.
pub struct Question {
    job: Job,
    question: String,
    choices: Vec<(String, Outcome)>,
    /// The diff the step came from.
    back: Option<Back>,
}

/// A line blame hint Revs handed out, for resolving its hover card.
struct HintRecord {
    uri: String,
    line: u32,
    version: u64,
}

pub struct Server {
    out: BufWriter<io::Stdout>,
    /// Opt-in protocol trace (`touch ~/Library/Caches/Revs/trace`), for debugging.
    trace: Option<std::fs::File>,
    events: mpsc::Sender<Event>,
    /// Background work: blames and hover cards.
    pool: Pool,
    /// Commands, so a step never waits behind background blames.
    commands: Pool,
    settings: Settings,
    toggle: Toggle,
    root: PathBuf,
    /// The file a single-file project shows (Zed opens diffs in its window only when
    /// given that file).
    single_file: Option<PathBuf>,
    /// "Revs: Open Commit on GitHub" (the host of the project's remote).
    open_commit: String,
    docs: HashMap<String, Doc>,
    /// Open documents by path (Zed's URIs and Revs's may encode a path differently).
    paths: HashMap<PathBuf, String>,
    /// Repositories by the folder of a file opened from them.
    repos: HashMap<PathBuf, Arc<Repo>>,
    /// The document where the cursor last moved.
    active: Option<String>,
    /// Documents in the order their cursors last moved (most recent first).
    recent: VecDeque<String>,
    last_report: Option<LastReport>,
    /// Reports are part of a refresh burst until then.
    burst_until: Option<Instant>,
    next_id: i64,
    inlay_refresh: Refresh,
    /// Hover cards by line, with when they were made (their links expire after a day).
    tooltips: HashMap<String, (Arc<str>, Instant)>,
    tooltip_order: VecDeque<String>,
    /// Hints handed out, by the id in their `data`.
    hints: HashMap<u64, HintRecord>,
    hint_order: VecDeque<u64>,
    next_hint: u64,
    /// Open "which line?" questions, by request id.
    choices: HashMap<i64, Question>,
    /// Blames of recently closed files, reused if one is reopened unchanged.
    closed: VecDeque<ClosedBlame>,
    /// When to hand freed memory back to the system (after bursts of large allocations).
    relief_at: Option<Instant>,
    shutdown: bool,
}

struct ClosedBlame {
    path: PathBuf,
    head: Option<String>,
    hash: u64,
    blame: Blame,
}

unsafe extern "C" {
    fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
}

/// Whether the file on disk uses CRLF line endings (checks the first 64 KB).
fn uses_crlf(path: &Path) -> bool {
    use std::io::Read as _;
    let mut head = Vec::with_capacity(64 * 1024);
    std::fs::File::open(path)
        .and_then(|file| file.take(64 * 1024).read_to_end(&mut head))
        .is_ok_and(|_| head.windows(2).any(|pair| pair == b"\r\n"))
}

/// The document text with the line endings the file has on disk.
fn disk_text(doc: &Doc) -> std::borrow::Cow<'_, str> {
    let text = doc.text.as_str();
    match &doc.kind {
        Kind::Working(working) if working.crlf && !text.contains('\r') => {
            std::borrow::Cow::Owned(text.replace('\n', "\r\n"))
        }
        _ => std::borrow::Cow::Borrowed(text),
    }
}

/// Whether a document reads what its file holds on disk (reloaded rather than typed in).
fn same_as_disk(doc: &Doc) -> bool {
    let Kind::Working(working) = &doc.kind else {
        return false;
    };
    std::fs::read(working.repo.root.join(&working.rel)).is_ok_and(|disk| disk == disk_text(doc).as_bytes())
}

fn text_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

pub fn run() {
    view::record_zed_cli_from_parent();
    std::thread::spawn(view::clean_old_snapshots);

    let (events, receiver) = mpsc::channel();
    let want_events = events.clone();
    share::watch(move |id, path| {
        let _ = want_events.send(Event::Want { id, path });
    });
    let reader_events = events.clone();
    std::thread::spawn(move || {
        // However the reader stops (end of input, a bad frame, a panic), the server stops
        // with it rather than waiting for messages that can't come.
        struct EndOnDrop(mpsc::Sender<Event>);
        impl Drop for EndOnDrop {
            fn drop(&mut self) {
                let _ = self.0.send(Event::Eof);
            }
        }
        let _end = EndOnDrop(reader_events.clone());
        let mut reader = BufReader::new(io::stdin().lock());
        while let Ok(Some(message)) = rpc::read_message(&mut reader) {
            if reader_events.send(Event::Message(message)).is_err() {
                break;
            }
        }
    });

    let trace = view::trace_enabled()
        .then(|| {
            use std::os::unix::fs::OpenOptionsExt as _;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(view::cache_dir().join("trace.log"))
                .ok()
        })
        .flatten();
    let mut server = Server {
        out: BufWriter::new(io::stdout()),
        trace,
        events,
        pool: Pool::new(2),
        commands: Pool::new(1),
        settings: Settings::default(),
        toggle: Toggle::new(),
        root: PathBuf::from("/"),
        single_file: None,
        open_commit: format!("{OPEN_COMMIT_PREFIX}GitHub"),
        docs: HashMap::new(),
        paths: HashMap::new(),
        repos: HashMap::new(),
        active: None,
        recent: VecDeque::new(),
        last_report: None,
        burst_until: None,
        next_id: 1,
        inlay_refresh: Refresh::default(),
        tooltips: HashMap::new(),
        tooltip_order: VecDeque::new(),
        hints: HashMap::new(),
        hint_order: VecDeque::new(),
        next_hint: 1,
        choices: HashMap::new(),
        closed: VecDeque::new(),
        relief_at: None,
        shutdown: false,
    };
    loop {
        let event = match server.relief_at {
            Some(at) => match receiver.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(event) => event,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    server.relief_at = None;
                    // Return pages freed by large transient buffers (file texts, git output).
                    unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match receiver.recv() {
                Ok(event) => event,
                Err(_) => break,
            },
        };
        let exit = matches!(event, Event::Eof);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            server.handle_event(event)
        }));
        if exit || matches!(outcome, Ok(true)) {
            break;
        }
    }
    view::remove_lease();
    share::remove_dirty_list();
}

fn params_uri(params: &Value) -> Option<&str> {
    params["textDocument"]["uri"].as_str()
}

fn position(value: &Value) -> (u32, u32) {
    (
        value["line"].as_u64().unwrap_or(0) as u32,
        value["character"].as_u64().unwrap_or(0) as u32,
    )
}

/// The answer to `workspace/executeCommand`, sent once the command is done (Zed's command
/// picker shows "Executing command…" until then, which also stops a second trigger).
/// Sent on drop if a job ends early.
pub struct Reply {
    id: Option<Value>,
    events: mpsc::Sender<Event>,
}

impl Reply {
    fn done(mut self) {
        self.send();
    }

    fn send(&mut self) {
        if let Some(id) = self.id.take() {
            let _ = self.events.send(Event::Respond { id, result: Value::Null });
        }
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        self.send();
    }
}

/// Everything a background command needs to finish: open a diff, or say why not.
#[derive(Clone)]
pub struct Job {
    repo: Arc<Repo>,
    root: PathBuf,
    /// The file a single-file project shows (passed to Zed with each diff).
    single_file: Option<PathBuf>,
    /// The file's current path in the repository.
    file: String,
    /// Unsaved text of the working file, if it's open.
    working: Option<Arc<[u8]>>,
    /// The working file, when it isn't open in this process (another project folder's
    /// Revs may have it, with unsaved changes).
    elsewhere: Option<PathBuf>,
    /// Where the step started, for the card: "parser.rs line 214".
    source: String,
    events: mpsc::Sender<Event>,
}

/// "parser.rs line 214" (`what` is "line", "removed line", …; `line` 0-based).
fn place(path: &str, what: &str, line: u32) -> String {
    format!("{} {what} {}", file_name(path), line + 1)
}

/// "parser.rs lines 3–9".
fn place_range(path: &str, what: &str, first: u32, last: u32) -> String {
    format!("{} {what} {}–{}", file_name(path), first + 1, last + 1)
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

impl Job {
    fn say(&self, message: impl Into<String>) {
        let _ = self.events.send(Event::ShowMessage(message.into()));
    }

    /// Fetches the working file's unsaved text from the Revs process that has it open.
    fn fetch_working(&mut self) {
        if self.working.is_none()
            && let Some(path) = self.elsewhere.take()
        {
            self.working = share::unsaved_text(&path).map(Arc::from);
        }
    }

    /// Carries out a step's outcome. `back` is the diff the step came from (none for a
    /// step from the working file).
    fn finish(&self, blobs: &Blobs, outcome: io::Result<Outcome>, back: Option<Back>, card: bool, reply: Reply) {
        match outcome {
            Ok(Outcome::Open {
                left,
                mut right,
                line,
                focus,
                note,
            }) => {
                right.describe(&self.repo);
                let opened = view::show(
                    blobs,
                    &self.root,
                    &self.file,
                    &left,
                    &right,
                    line,
                    focus,
                    self.working.as_deref(),
                    back,
                    self.single_file.as_deref(),
                );
                match opened {
                    Ok((pair, character)) => {
                        let _ = self.events.send(Event::Opened {
                            pair: Box::new(pair),
                            line,
                            character,
                            focus,
                        });
                        if card {
                            let _ = self.events.send(Event::Card(self.card(&left, &right, note.as_deref())));
                        } else if let Some(note) = note {
                            self.say(capitalize(&note));
                        }
                    }
                    Err(error) => self.say(format!("Couldn't open the diff: {error}")),
                }
                reply.done();
            }
            Ok(Outcome::Stay(message)) => {
                self.say(message);
                reply.done();
            }
            Ok(Outcome::Choose { question, choices }) => {
                // Answer the command now: the question waits for a click.
                reply.done();
                let _ = self.events.send(Event::Choose(Box::new(Question {
                    job: self.clone(),
                    question,
                    choices,
                    back,
                })));
            }
            Err(error) => {
                self.say(explain(&error));
                reply.done();
            }
        }
    }

    /// "**8a7b6c5** Fix typo (#12)  ↵ You · Jan 5, 2026 (9 months ago) · from
    /// parser.rs line 214 · GitHub"
    fn card(&self, left: &Side, right: &Side, note: Option<&str>) -> String {
        let mut out = match right.rev.as_str() {
            view::WORKING => "**Uncommitted changes**".to_string(),
            view::EMPTY => format!(
                "**{}** · {}",
                format::short_sha(&left.rev),
                format::escape_markdown(note.unwrap_or("the file is gone after this"))
            ),
            sha => {
                let who = if self.repo.is_you(&right.author, &right.mail) {
                    "You".to_string()
                } else {
                    format::escape_markdown(&right.author)
                };
                let mut out = format!(
                    "**{}** {}  \n{who} · {} ({})",
                    format::short_sha(sha),
                    format::escape_markdown(&right.summary),
                    format::absolute_date(right.time),
                    format::relative_time(right.time, format::now()),
                );
                if let Some(note) = note {
                    out.push_str(&format!(" · {}", format::escape_markdown(note)));
                }
                out
            }
        };
        if !self.source.is_empty() {
            out.push_str(&format!(" · {}", format::escape_markdown(&self.source)));
        }
        if right.is_commit()
            && let Some(remote) = self.repo.remote()
        {
            out.push_str(&format!(" · [{}]({})", remote.name(), remote.commit_url(&right.rev)));
        }
        out
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Git's errors in the user's terms where they have an obvious meaning.
fn explain(error: &io::Error) -> String {
    let text = error.to_string();
    if text.contains("no such path") && text.contains("in HEAD") {
        "This file isn't committed yet".into()
    } else if text.contains("no such ref") || text.contains("bad revision 'HEAD'") || text.contains("can't read HEAD") {
        "This repository has no commits yet".into()
    } else if text.contains("has only") && text.contains("lines") {
        "There's no line here".into()
    } else {
        text
    }
}

/// Which line a cursor on the right side of a diff stands for. A cursor on the left side
/// arrives as the right row it lines up with; on a removed line (left only) as column 0
/// of the first right line after the removal, or — below the last line of a file without
/// a final newline — as the end of that line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    /// A right line, known to be meant (Revs put the cursor there).
    Right(u32),
    /// Unchanged line: left `left`, right `right`.
    Unchanged { left: u32, right: u32 },
    /// A changed line shown beside an older one (either may be meant).
    Paired { left: u32, right: u32 },
    /// A new line with nothing beside it.
    Added { right: u32 },
    /// Lines `first..=last` removed on the left (one of them is meant).
    Removed { first: u32, last: u32 },
    /// Right lines `first..=last` selected.
    Section { first: u32, last: u32 },
    /// The end of the last line of a file without a final newline, when lines were removed
    /// below it: Zed puts three different clicks there (the line itself, the old line
    /// beside it, the removed rows below), so each of them may be meant.
    EndOfLast {
        right: u32,
        left: Option<u32>,
        first: u32,
        last: u32,
    },
}

fn classify(hunks: &[diff::Hunk], cursor: Cursor, right_text: &[u8]) -> Target {
    let line = cursor.line;
    if cursor.last_line > line {
        return Target::Section {
            first: line,
            last: cursor.last_line,
        };
    }
    let mirrored = cursor.empty && !cursor.placed;
    let mut delta: i64 = 0;
    for hunk in hunks {
        let (old_count, new_count) = (hunk.old_count, hunk.new_count);
        let (old_first, new_first) = (hunk.old_first(), hunk.new_first());
        let new_end = new_first + new_count;
        let removed = || Target::Removed {
            first: old_first + new_count,
            last: old_first + old_count - 1,
        };
        if line < new_first {
            break;
        }
        if line < new_end {
            let index = line - new_first;
            // Rows removed below the last line of a file without a final newline: Zed
            // mirrors a click there to the end of that line.
            if old_count > new_count
                && mirrored
                && cursor.character > 0
                && line + 1 == new_end
                && !right_text.ends_with(b"\n")
                && history::line_count(right_text) == new_end as usize
                && line_utf16_len(right_text, line) == Some(cursor.character)
            {
                return Target::EndOfLast {
                    right: line,
                    left: (index < old_count).then_some(old_first + index),
                    first: old_first + new_count,
                    last: old_first + old_count - 1,
                };
            }
            return if index < old_count {
                Target::Paired {
                    left: old_first + index,
                    right: line,
                }
            } else {
                Target::Added { right: line }
            };
        }
        if old_count > new_count && line == new_end && mirrored && cursor.character == 0 {
            return removed();
        }
        delta += old_count as i64 - new_count as i64;
    }
    Target::Unchanged {
        left: (line as i64 + delta).max(0) as u32,
        right: line,
    }
}

fn utf16_len(text: &str) -> u32 {
    text.encode_utf16().count() as u32
}

/// UTF-16 length of 0-based `line` of `text` (without its line ending).
fn line_utf16_len(text: &[u8], line: u32) -> Option<u32> {
    text.split(|&byte| byte == b'\n')
        .nth(line as usize)
        .map(|content| utf16_len(String::from_utf8_lossy(content).trim_end_matches('\r')))
}

fn line_of(text: &[u8], line: u32) -> String {
    text.split(|&byte| byte == b'\n')
        .nth(line as usize)
        .map(|content| String::from_utf8_lossy(content).trim_end_matches('\r').to_string())
        .unwrap_or_default()
}

/// Whether a column on this line counts characters the way Zed's display does (no tabs,
/// which Zed expands, and no multi-byte characters, which it counts in bytes).
fn plain_columns(text: &str) -> bool {
    text.is_ascii() && !text.contains('\t')
}

/// The commit an outcome would show (to tell whether two readings agree).
fn shown_commit(outcome: &Outcome) -> Option<&str> {
    match outcome {
        Outcome::Open { right, .. } => Some(&right.rev),
        _ => None,
    }
}

/// A step from a Revs diff, and the words for where it started ("from
/// parser.rs line 214").
fn navigate_view(
    job: &Job,
    blobs: &Blobs,
    pair: &Pair,
    cursor: Cursor,
    focus: Option<Focus>,
    forward: bool,
) -> io::Result<(Outcome, String, Option<Focus>)> {
    let repo = blobs.repo();
    let right_text = std::fs::read(&pair.right_path)?;
    let left_text = view::contents(blobs, &pair.left, None);
    let (left, right) = (&pair.left, &pair.right);
    let hunks = std::cell::OnceCell::new();
    let hunks = || {
        hunks.get_or_init(|| diff::hunks(&String::from_utf8_lossy(&left_text), &String::from_utf8_lossy(&right_text)))
    };
    let target = match focus.filter(|_| cursor.placed) {
        Some(Focus::Right(line)) => Target::Right(line),
        Some(Focus::Left(line)) => Target::Removed { first: line, last: line },
        None => classify(hunks(), cursor, &right_text),
    };
    let on_right = |what: &str, line: u32| format!("from {}", place(&right.path, what, line));
    let on_left = |what: &str, line: u32| format!("from {}", place(&left.path, what, line));
    let right_focus = |line: u32| Some(Focus::Right(line));
    // The first line of a removed block with some content stands for the block.
    let stand_in = |first: u32, last: u32| {
        let lines = history::lines_of(&left_text);
        (first..=last)
            .find(|&line| {
                lines
                    .get(line as usize)
                    .is_some_and(|content| !diff::is_trivial(&String::from_utf8_lossy(content)))
            })
            .unwrap_or(first)
    };
    // A working-tree right side: its next change is past the commit it is compared with,
    // once the working changes have been committed.
    let next_of_working = |left_line: Option<u32>| -> io::Result<Outcome> {
        if repo.head().as_deref() == Some(left.rev.as_str()) {
            return Ok(Outcome::Stay(
                "This is the working file: there's nothing newer. Revs: Open Working File opens it".into(),
            ));
        }
        match left_line {
            Some(line) => history::next_from(blobs, left, line, job.working.as_deref()),
            None => Ok(Outcome::Stay("This line isn't committed yet".into())),
        }
    };
    let older_line = |line: u32| -> Option<u32> {
        match diff::new_to_old(hunks(), line) {
            diff::NewToOld::Changed(older) | diff::NewToOld::Unchanged(older) => Some(older),
            diff::NewToOld::Added => None,
        }
    };
    let next_right = |line: u32| -> io::Result<Outcome> {
        if right.rev == view::WORKING {
            next_of_working(older_line(line))
        } else {
            history::next_from(blobs, right, line, job.working.as_deref())
        }
    };
    Ok(match (target, forward) {
        (Target::Right(line) | Target::Unchanged { right: line, .. } | Target::Added { right: line }, false) => (
            history::previous_on_right(blobs, left, right, &right_text, line)?,
            on_right("line", line),
            right_focus(line),
        ),
        (Target::Right(line) | Target::Unchanged { right: line, .. } | Target::Added { right: line }, true) => {
            (next_right(line)?, on_right("line", line), right_focus(line))
        }
        (Target::Removed { first, last }, false) => (
            history::previous_on_left(blobs, left, right, first, last)?,
            if first == last {
                on_left("removed line", first)
            } else {
                format!("from {}", place_range(&left.path, "removed lines", first, last))
            },
            Some(Focus::Left(first)),
        ),
        (Target::Removed { first, last }, true) => {
            let line = stand_in(first, last);
            (
                history::next_on_left(blobs, left, right, &right_text, line, job.working.as_deref())?,
                on_left("removed line", line),
                Some(Focus::Left(line)),
            )
        }
        (Target::Section { first, last }, false) => (
            history::previous_section_on_right(blobs, left, right, &right_text, first, last)?,
            format!("from {}", place_range(&right.path, "lines", first, last)),
            right_focus(first),
        ),
        (Target::Section { first, last }, true) => (
            if right.rev == view::WORKING {
                next_of_working(older_line(first))?
            } else {
                history::next_section(blobs, right, first, last, job.working.as_deref())?
            },
            format!("from {}", place_range(&right.path, "lines", first, last)),
            right_focus(first),
        ),
        (
            Target::EndOfLast {
                right: new,
                left: old,
                first,
                last,
            },
            forward,
        ) => {
            let new_text = line_of(&right_text, new);
            let mut readings = vec![(
                format!("Line {}: {}", new + 1, format::truncate(new_text.trim(), 40)),
                if forward {
                    next_right(new)?
                } else {
                    history::previous_on_right(blobs, left, right, &right_text, new)?
                },
            )];
            if let Some(old) = old {
                let old_text = line_of(&left_text, old);
                if diff::similarity(&old_text, &new_text) < diff::SIMILAR {
                    readings.push((
                        format!("Old line {}: {}", old + 1, format::truncate(old_text.trim(), 40)),
                        if forward {
                            history::next_on_left(blobs, left, right, &right_text, old, job.working.as_deref())?
                        } else {
                            history::previous_on_left(blobs, left, right, old, old)?
                        },
                    ));
                }
            }
            let removed_text = line_of(&left_text, first);
            readings.push((
                if first == last {
                    format!("Removed line {}: {}", first + 1, format::truncate(removed_text.trim(), 40))
                } else {
                    format!("Removed lines {}–{}", first + 1, last + 1)
                },
                if forward {
                    history::next_on_left(blobs, left, right, &right_text, stand_in(first, last), job.working.as_deref())?
                } else {
                    history::previous_on_left(blobs, left, right, first, last)?
                },
            ));
            // Readings that agree are one; the line's own goes first.
            let mut distinct: Vec<(String, Outcome)> = Vec::new();
            for (label, outcome) in readings {
                if !distinct.iter().any(|(_, known)| shown_commit(known) == shown_commit(&outcome)) {
                    distinct.push((label, outcome));
                }
            }
            if distinct.len() == 1 {
                let (_, outcome) = distinct.remove(0);
                return Ok((outcome, on_right("line", new), right_focus(new)));
            }
            (
                Outcome::Choose {
                    question: "Several lines meet at the end of the file. Which one?".into(),
                    choices: distinct,
                },
                on_right("line", new),
                right_focus(new),
            )
        }
        (Target::Paired { left: old, right: new }, forward) => {
            let old_text = line_of(&left_text, old);
            let new_text = line_of(&right_text, new);
            // Similar lines give the same answer either way; a cursor past the end of the
            // old line can only be on the right (columns compare only on plain lines).
            let right_side = diff::similarity(&old_text, &new_text) >= diff::SIMILAR
                || (plain_columns(&old_text) && plain_columns(&new_text) && cursor.character > utf16_len(&old_text));
            let by_right = if forward {
                next_right(new)?
            } else {
                history::previous_on_right(blobs, left, right, &right_text, new)?
            };
            if right_side {
                return Ok((by_right, on_right("line", new), right_focus(new)));
            }
            let by_left = if forward {
                history::next_on_left(blobs, left, right, &right_text, old, job.working.as_deref())?
            } else {
                history::previous_on_left(blobs, left, right, old, old)?
            };
            if shown_commit(&by_left) == shown_commit(&by_right) {
                return Ok((by_right, on_right("line", new), right_focus(new)));
            }
            (
                Outcome::Choose {
                    question: "Two different lines sit here. Which one?".into(),
                    choices: vec![
                        (
                            format!("Old line {}: {}", old + 1, format::truncate(old_text.trim(), 40)),
                            by_left,
                        ),
                        (
                            format!("New line {}: {}", new + 1, format::truncate(new_text.trim(), 40)),
                            by_right,
                        ),
                    ],
                },
                on_right("line", new),
                right_focus(new),
            )
        }
    })
}

impl Server {
    /// Returns true when the server should exit.
    fn handle_event(&mut self, event: Event) -> bool {
        match event {
            Event::Message(message) => return self.handle_message(message),
            Event::Eof => return true,
            Event::Blame {
                uri,
                generation,
                head,
                blame,
            } => self.blame_done(&uri, generation, head, blame),
            Event::QuickBlame {
                uri,
                generation,
                version,
                line,
                blame,
            } => {
                let mut chase = false;
                if let Some(doc) = self.docs.get_mut(&uri)
                    && let Kind::Working(working) = &mut doc.kind
                {
                    working.quick_in_flight = false;
                    if doc.generation.load(Ordering::Relaxed) == generation
                        && working.blame.is_none()
                    {
                        if doc.cursor.map(|cursor| cursor.line) == Some(line) && doc.version == version {
                            if let Some(blame) = blame {
                                working.quick = Some((line, blame));
                                self.refresh_inlays();
                            }
                        } else {
                            // The cursor moved on while git ran.
                            chase = true;
                        }
                    }
                }
                if chase {
                    self.quick_blame(&uri);
                }
                self.schedule_relief();
            }
            Event::Respond { id, result } => self.respond(id, result),
            Event::ShowDocument {
                uri,
                line,
                character,
                external,
            } => {
                let mut params = json!({ "uri": uri, "takeFocus": true, "external": external });
                if let Some(line) = line {
                    let point = json!({ "line": line, "character": character });
                    params["selection"] = json!({ "start": point, "end": point });
                }
                self.request("window/showDocument", Some(params));
            }
            Event::ShowMessage(text) => {
                self.notify(
                    "window/showMessage",
                    json!({ "type": 3, "message": format::escape_markdown(&text) }),
                );
                // Commands end with a message or an open diff; both follow large transient
                // buffers (file texts, git output).
                self.schedule_relief();
            }
            Event::Card(markdown) => {
                self.notify("window/showMessage", json!({ "type": 3, "message": markdown }));
            }
            Event::Opened {
                pair,
                line,
                character,
                focus,
            } => {
                // Already open in another tab: Zed shares the buffer and sends no didOpen,
                // so take over the new record (sides and trail) here. Otherwise didOpen
                // reads it from beside the snapshot, in whichever Revs process Zed
                // hands the file to.
                if let Some(uri) = self.paths.get(&pair.right_path).cloned()
                    && let Some(doc) = self.docs.get_mut(&uri)
                {
                    doc.cursor = Some(Cursor {
                        placed: true,
                        ..Cursor::at(line, character)
                    });
                    doc.focus = Some(focus);
                    doc.record_modified = view::record_modified(&pair.right_path);
                    doc.kind = Kind::View(pair);
                    self.make_active(&uri);
                }
                self.schedule_relief();
            }
            Event::Choose(question) => {
                let actions: Vec<Value> = question
                    .choices
                    .iter()
                    .map(|(title, _)| json!({ "title": title }))
                    .collect();
                let id = self.request(
                    "window/showMessageRequest",
                    Some(json!({ "type": 3, "message": question.question, "actions": actions })),
                );
                self.choices.insert(id, *question);
                // Unanswered questions (closed cards) never come back; keep only a few.
                if self.choices.len() > 8
                    && let Some(&oldest) = self.choices.keys().min()
                {
                    self.choices.remove(&oldest);
                }
            }
            Event::Tooltip { key, markdown } => {
                self.cache_tooltip(key, markdown);
            }
            Event::Want { id, path } => {
                if let Some(doc) = self.paths.get(&path).and_then(|uri| self.docs.get(uri))
                    && doc.dirty
                    && matches!(doc.kind, Kind::Working(_))
                {
                    share::answer(&id, disk_text(doc).as_bytes());
                }
            }
        }
        false
    }

    fn log(&mut self, direction: &str, message: &Value) {
        if let Some(trace) = &mut self.trace {
            use std::io::Write as _;
            // The trace stays a few MB however long the server runs.
            static LINES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if LINES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 1000 == 999
                && trace.metadata().is_ok_and(|meta| meta.len() > 8 << 20)
            {
                let _ = trace.set_len(0);
            }
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default();
            let summary = message["method"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| format!("response {}", message["id"]));
            let detail = match message["method"].as_str() {
                Some("textDocument/codeAction") => format!(
                    "{} {}",
                    message["params"]["range"]["start"], message["params"]["range"]["end"]
                ),
                Some("textDocument/linkedEditingRange") => message["params"]["position"].to_string(),
                Some("textDocument/inlayHint") => message["params"]["range"].to_string(),
                Some("workspace/executeCommand") => message["params"]["command"].to_string(),
                _ => String::new(),
            };
            let _ = writeln!(
                trace,
                "{}.{:03} {} {} {} {}",
                now.as_secs(),
                now.subsec_millis(),
                std::process::id(),
                direction,
                summary,
                detail
            );
        }
    }

    fn handle_message(&mut self, message: Value) -> bool {
        self.log("<-", &message);
        let method = message["method"].as_str().map(str::to_string);
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            (Some(method), Some(id)) => self.handle_request(&method, id, params),
            (Some(method), None) => return self.handle_notification(&method, params),
            (None, Some(id)) => self.handle_response(&id, &message["result"]),
            (None, None) => {}
        }
        false
    }

    fn handle_request(&mut self, method: &str, id: Value, params: Value) {
        if self.shutdown && method != "shutdown" {
            self.respond_error(id, -32600, "server is shutting down");
            return;
        }
        match method {
            "initialize" => {
                let result = self.initialize(&params);
                self.respond(id, result);
            }
            "shutdown" => {
                self.shutdown = true;
                self.respond(id, Value::Null);
            }
            "textDocument/inlayHint" => {
                let hints = self.inlay_hints(&params);
                self.respond(id, hints);
            }
            "inlayHint/resolve" => self.resolve_hint(id, params),
            "textDocument/codeAction" => {
                // Revs offers no code actions; the request tells it where the cursor is.
                // Requests for particular kinds (on format, on save) aren't cursor moves.
                self.respond(id, json!([]));
                let for_kinds = params["context"]["only"].as_array().is_some_and(|kinds| !kinds.is_empty());
                if !for_kinds && let Some(uri) = params_uri(&params) {
                    let (line, character) = position(&params["range"]["start"]);
                    let (end_line, end_character) = position(&params["range"]["end"]);
                    let empty = (line, character) == (end_line, end_character);
                    // A selection ending at the start of a line doesn't include that line.
                    let last_line = if end_line > line && end_character == 0 { end_line - 1 } else { end_line };
                    let uri = uri.to_string();
                    self.track_cursor(
                        &uri,
                        Cursor {
                            last_line: last_line.max(line),
                            empty,
                            ..Cursor::at(line, character)
                        },
                    );
                }
            }
            "textDocument/linkedEditingRange" => {
                self.respond(id, Value::Null);
                if let Some(uri) = params_uri(&params) {
                    let (line, character) = position(&params["position"]);
                    let uri = uri.to_string();
                    self.track_cursor(&uri, Cursor::at(line, character));
                }
            }
            "workspace/executeCommand" => self.execute_command(id, &params),
            _ => self.respond_error(id, -32601, "method not found"),
        }
    }

    fn handle_notification(&mut self, method: &str, params: Value) -> bool {
        match method {
            "exit" => return true,
            "textDocument/didOpen" => {
                let mut params = params;
                self.did_open(&mut params);
                self.schedule_relief();
            }
            "textDocument/didChange" => {
                let mut params = params;
                self.did_change(&mut params);
            }
            "textDocument/didSave" => {
                if let Some(doc) = params_uri(&params).and_then(|uri| self.docs.get_mut(uri))
                    && std::mem::take(&mut doc.dirty)
                {
                    self.list_dirty();
                }
                // Saving changes neither the text nor HEAD, so the blame stays right; only
                // lines marked edited (perhaps typed back to what was committed) need a
                // fresh blame to be exact again.
                if let Some(uri) = params_uri(&params)
                    && let Some(doc) = self.docs.get_mut(uri)
                    && let Kind::Working(working) = &mut doc.kind
                    && working
                        .blame
                        .as_ref()
                        .is_some_and(|blame| blame.lines.iter().any(|line| line.commit == EDITED))
                {
                    working.stale = true;
                    let uri = uri.to_string();
                    if self.annotation() && self.active.as_deref() == Some(uri.as_str()) {
                        self.ensure_blame(&uri);
                    }
                }
            }
            "textDocument/didClose" => {
                if let Some(uri) = params_uri(&params)
                    && let Some(doc) = self.docs.remove(uri)
                {
                    doc.generation.store(next_generation(), Ordering::Relaxed);
                    self.paths.retain(|_, known| known != uri);
                    self.recent.retain(|known| known != uri);
                    if self.active.as_deref() == Some(uri) {
                        // Zed shows the previously used tab next; follow it.
                        self.active = self.recent.front().cloned();
                    }
                    if matches!(doc.kind, Kind::View(_)) {
                        self.update_lease();
                    }
                    if doc.dirty {
                        self.list_dirty();
                    }
                    self.prune_repos();
                    if let Kind::Working(working) = doc.kind
                        && let Some(blame) = working.blame
                        && !working.stale
                        && !working.pending
                        // Lines marked edited only become exact again after a fresh blame.
                        && !blame.lines.iter().any(|line| line.commit == EDITED)
                        && let Some(path) = format::uri_to_path(uri)
                    {
                        self.closed.push_front(ClosedBlame {
                            path,
                            head: working.blame_head,
                            hash: text_hash(doc.text.as_str()),
                            blame,
                        });
                        self.closed.truncate(4);
                    }
                    self.schedule_relief();
                }
            }
            "workspace/didChangeConfiguration" => {
                // Zed sends this to every server whenever any diff tab closes; only act
                // on real changes.
                let settings = &params["settings"];
                let settings = settings.get("revs").unwrap_or(settings);
                if settings.is_object() {
                    let before = self.settings.clone();
                    self.settings.apply(settings);
                    if self.settings != before {
                        self.refresh_inlays();
                    }
                }
            }
            _ => {}
        }
        false
    }

    fn handle_response(&mut self, id: &Value, result: &Value) {
        let Some(id) = id.as_i64() else {
            return;
        };
        if self.inlay_refresh.in_flight == Some(id) {
            self.inlay_refresh.in_flight = None;
            if std::mem::take(&mut self.inlay_refresh.queued) {
                self.refresh_inlays();
            }
        } else if let Some(Question { job, choices, back, .. }) = self.choices.remove(&id) {
            let title = result["title"].as_str().unwrap_or_default();
            if let Some((_, outcome)) = choices.into_iter().find(|(choice, _)| choice == title) {
                let card = self.settings.step_card;
                let reply = Reply {
                    id: None,
                    events: self.events.clone(),
                };
                self.commands.spawn(move || {
                    let blobs = Blobs::new(&job.repo);
                    job.finish(&blobs, Ok(outcome), back, card, reply);
                });
            }
        }
    }

    fn initialize(&mut self, params: &Value) -> Value {
        let root = params["rootUri"]
            .as_str()
            .and_then(format::uri_to_path)
            .or_else(|| {
                params["workspaceFolders"][0]["uri"]
                    .as_str()
                    .and_then(format::uri_to_path)
            })
            .or_else(|| params["rootPath"].as_str().map(PathBuf::from));
        if let Some(root) = root {
            self.root = root;
        }
        // A single-file project names the file itself as its workspace folder (and its
        // folder as the root).
        self.single_file = params["workspaceFolders"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|folder| folder["uri"].as_str().and_then(format::uri_to_path))
            .find(|path| path.is_file());
        self.settings.apply(&params["initializationOptions"]);
        // Kept like any file's repository, so its remote is looked up only once.
        let root = self.root.clone();
        if let Some(repo) = self.repo_in(&root)
            && let Some(remote) = repo.remote()
        {
            self.open_commit = format!("{OPEN_COMMIT_PREFIX}{}", remote.name());
        }
        let mut capabilities = json!({
            "positionEncoding": "utf-16",
            "textDocumentSync": {
                "openClose": true,
                "change": 2,
                "save": { "includeText": false },
            },
            "inlayHintProvider": { "resolveProvider": true },
            // No kinds: Zed then asks only for the code actions at the cursor (which tell
            // Revs where it is), never for on-format or on-save kinds.
            "codeActionProvider": { "codeActionKinds": [] },
            "executeCommandProvider": {
                "commands": [PREVIOUS, TOGGLE_BLAME, NEXT, OPEN_FILE, self.open_commit.as_str()],
            },
        });
        if self.settings.fast_cursor {
            capabilities["linkedEditingRangeProvider"] = json!(true);
        }
        json!({
            "capabilities": capabilities,
            "serverInfo": { "name": "revs", "version": env!("CARGO_PKG_VERSION") },
        })
    }

    /// Whether the current-line blame annotation is on.
    fn annotation(&self) -> bool {
        self.toggle.value.unwrap_or(self.settings.annotation)
    }

    // ---- documents -------------------------------------------------------------------

    fn repo_for(&mut self, path: &Path) -> Option<Arc<Repo>> {
        self.repo_in(path.parent()?)
    }

    /// The repository a folder is in.
    fn repo_in(&mut self, dir: &Path) -> Option<Arc<Repo>> {
        let dir = dir.to_path_buf();
        if let Some(repo) = self.repos.get(&dir) {
            return Some(repo.clone());
        }
        // Finding none takes a few `stat` calls, so folders outside repositories aren't
        // remembered.
        let found = Repo::discover(&dir)?;
        // Share one Repo per working tree.
        let repo = self
            .repos
            .values()
            .find(|known| known.root == found.root)
            .cloned()
            .unwrap_or(found);
        self.repos.insert(dir, repo.clone());
        repo.into()
    }

    /// Forgets folders no open document is in (keeps the map small in long sessions).
    fn prune_repos(&mut self) {
        if self.repos.len() <= 64 {
            return;
        }
        let used: std::collections::HashSet<&Path> = self.paths.keys().filter_map(|path| path.parent()).collect();
        self.repos.retain(|dir, _| used.contains(dir.as_path()));
    }

    /// Tells other Revs processes which snapshot files this window's diffs show, so
    /// their cleanup keeps them.
    fn update_lease(&self) {
        let files: Vec<PathBuf> = self
            .docs
            .values()
            .filter_map(|doc| match &doc.kind {
                Kind::View(pair) => Some(pair.files()),
                _ => None,
            })
            .flatten()
            .collect();
        view::write_lease(&files);
    }

    fn did_open(&mut self, params: &mut Value) {
        let Some(document) = params.get_mut("textDocument") else {
            return;
        };
        let Some(uri) = document["uri"].as_str().map(str::to_string) else {
            return;
        };
        let Value::String(mut text) = document["text"].take() else {
            return;
        };
        let Some(path) = format::uri_to_path(&uri) else {
            return;
        };
        let kind = if let Some(pair) = Pair::load(&path) {
            // Revs reads its own snapshots from disk; don't keep a copy.
            text = String::new();
            self.repo_for(&pair.working_path());
            Kind::View(Box::new(pair))
        } else if view::is_snapshot(&path) {
            text = String::new();
            Kind::Plain
        } else if let Some(repo) = self.repo_for(&path)
            && let Some(rel) = repo.relative(&path)
            && !rel.starts_with(".git/")
        {
            let mut working = Working {
                repo,
                rel,
                blame: None,
                blame_head: None,
                attempted: false,
                stale: false,
                pending: false,
                edits_during_blame: Vec::new(),
                quick: None,
                quick_line: None,
                quick_in_flight: false,
                snapshot: None,
                crlf: uses_crlf(&path),
            };
            if let Some(index) = self.closed.iter().position(|closed| closed.path == path) {
                let closed = self.closed.remove(index).expect("index from position");
                if closed.hash == text_hash(&text) && closed.head == working.repo.head() {
                    working.blame = Some(closed.blame);
                    working.blame_head = closed.head;
                    working.attempted = true;
                }
            }
            Kind::Working(Box::new(working))
        } else {
            // Nothing reads the text of files outside a repository.
            text = String::new();
            Kind::Plain
        };
        // A diff a step just opened: the cursor is where the step put it. (The record
        // beside the snapshot says so, whichever Revs process ran the step.)
        let (mut cursor, mut focus) = (None, None);
        if let Kind::View(pair) = &kind
            && let Some(landing) = pair.landing
            && (view::now() - landing.at).abs() <= 30
        {
            cursor = Some(Cursor {
                placed: true,
                ..Cursor::at(landing.line, landing.character)
            });
            focus = Some(landing.focus);
        }
        let is_view = matches!(kind, Kind::View(_));
        let record_modified = match &kind {
            Kind::View(pair) => view::record_modified(&pair.right_path),
            _ => None,
        };
        self.paths.insert(path, uri.clone());
        self.docs.insert(
            uri.clone(),
            Doc {
                text: Text::new(text),
                version: 0,
                echo: None,
                record_modified,
                dirty: false,
                cursor,
                focus,
                kind,
                generation: Arc::new(AtomicU64::new(next_generation())),
            },
        );
        if cursor.is_some() {
            self.make_active(&uri);
        }
        if is_view {
            self.update_lease();
        }
        // No blame yet: Zed registers every buffer a multibuffer or search shows, and the
        // cursor report that follows a real visit starts one.
    }

    fn did_change(&mut self, params: &mut Value) {
        let Some(uri) = params_uri(params).map(str::to_string) else {
            return;
        };
        let Some(doc) = self.docs.get_mut(&uri) else {
            return;
        };
        if matches!(doc.kind, Kind::View(_)) {
            return; // read-only snapshots; their text isn't kept
        }
        let Value::Array(changes) = params["contentChanges"].take() else {
            return;
        };
        // Files outside a repository keep no text; only their lines are followed.
        let keep_text = !matches!(doc.kind, Kind::Plain);
        doc.version += 1;
        let newly_dirty = !doc.dirty && matches!(doc.kind, Kind::Working(_));
        doc.dirty = true;
        let stored = doc.cursor.map(|cursor| (cursor.line, cursor.character));
        let mut replaced = false;
        // The lines the changes span once made, and where the first one began.
        let mut span: Option<(u32, u32, (u32, u32))> = None;
        for mut change in changes {
            if !change.is_object() {
                continue;
            }
            let edit: LineEdit = if change.get("range").is_some_and(|range| !range.is_null()) {
                let (start, end) = (position(&change["range"]["start"]), position(&change["range"]["end"]));
                let text = change["text"].as_str().unwrap_or_default();
                let inserted = text.matches('\n').count();
                let (first, last, began) = span.unwrap_or((start.0, start.0, start));
                span = Some((first.min(start.0), last.max(start.0 + inserted as u32), began));
                if keep_text {
                    doc.text.edit(start, end, text)
                } else {
                    LineEdit {
                        start: start.0 as usize,
                        removed: end.0.saturating_sub(start.0) as usize,
                        inserted,
                    }
                }
            } else {
                replaced = true;
                match change["text"].take() {
                    Value::String(text) if keep_text => doc.text.replace_all(text),
                    _ => continue,
                }
            };
            if let Some(cursor) = &mut doc.cursor {
                cursor.line = shift_line(cursor.line, edit);
                cursor.last_line = shift_line(cursor.last_line, edit).max(cursor.line);
            }
            if let Kind::Working(working) = &mut doc.kind {
                if let Some(blame) = &mut working.blame {
                    blame.apply_edit(edit);
                }
                if working.pending {
                    working.edits_during_blame.push(edit);
                }
                working.quick = None;
            }
        }
        if let Some((first, last, began)) = span.filter(|_| !replaced) {
            // Typing goes on from where it began.
            let typing = doc
                .echo
                .as_ref()
                .is_some_and(|echo| echo.at_cursor && echo.at.elapsed() < ECHO_WINDOW);
            doc.echo = Some(Echo {
                first,
                last,
                at: Instant::now(),
                at_cursor: typing || stored.is_none_or(|stored| stored == began),
                settled: None,
            });
        }
        if replaced {
            self.schedule_relief();
        }
        if newly_dirty {
            self.list_dirty();
        }
    }

    /// Tells other Revs processes which working files have unsaved changes here.
    fn list_dirty(&self) {
        let paths: Vec<PathBuf> = self
            .docs
            .values()
            .filter(|doc| doc.dirty)
            .filter_map(|doc| match &doc.kind {
                Kind::Working(working) => Some(working.repo.root.join(&working.rel)),
                _ => None,
            })
            .collect();
        share::list_dirty(&paths);
    }

    fn make_active(&mut self, uri: &str) {
        self.active = Some(uri.to_string());
        self.recent.retain(|known| known != uri);
        self.recent.push_front(uri.to_string());
        self.recent.truncate(32);
    }

    fn track_cursor(&mut self, uri: &str, cursor: Cursor) {
        if self.toggle.refresh() {
            // Line blame was switched in another window.
            self.refresh_inlays();
        }
        let now = Instant::now();
        self.refresh_view(uri, Some(cursor));
        let active = self.active.as_deref() == Some(uri);
        let after_edit = match self.echoed(uri, cursor, active) {
            Echoed::No => false,
            Echoed::User => true,
            Echoed::Background => {
                self.store_cursor(uri, cursor);
                return;
            }
            Echoed::OtherPane => return,
        };
        if !after_edit && self.in_burst(uri, active, now) {
            self.store_cursor(uri, cursor);
            return;
        }
        if !self.docs.contains_key(uri) {
            return;
        }
        let annotation = self.annotation();
        let moved = self.store_cursor(uri, cursor);
        let annotated = matches!(self.docs.get(uri).map(|doc| &doc.kind), Some(Kind::Working(_)));
        let switched = !active;
        let left_annotated = switched
            && self
                .active
                .as_ref()
                .and_then(|active| self.docs.get(active))
                .is_some_and(|doc| matches!(doc.kind, Kind::Working(_)));
        self.last_report = Some(LastReport {
            uri: uri.to_string(),
            at: now,
            before: switched.then(|| (self.active.clone(), self.recent.clone())),
        });
        self.make_active(uri);
        if !annotation || !((moved || switched) && annotated || left_annotated) {
            return;
        }
        if annotated {
            self.ensure_blame(uri);
            self.quick_blame(uri);
        }
        self.refresh_inlays();
    }

    /// Stores a reported cursor; true when it is on another line than before.
    fn store_cursor(&mut self, uri: &str, cursor: Cursor) -> bool {
        let Some(doc) = self.docs.get_mut(uri) else {
            return false;
        };
        let moved = doc.cursor.map(|old| old.line) != Some(cursor.line);
        let mut cursor = cursor;
        if let Some(old) = doc.cursor
            && old.placed
            && (old.line, old.character) == (cursor.line, cursor.character)
            && cursor.empty
        {
            // Zed reporting where Revs put the cursor, not a click.
            cursor.placed = true;
        } else {
            doc.focus = None;
        }
        doc.cursor = Some(cursor);
        moved
    }

    /// Who a report right after an edit of the document comes from.
    fn echoed(&mut self, uri: &str, cursor: Cursor, active: bool) -> Echoed {
        let Some(doc) = self.docs.get_mut(uri) else {
            return Echoed::No;
        };
        let Some((first, last, at_cursor)) = doc
            .echo
            .as_ref()
            .filter(|echo| echo.at.elapsed() < ECHO_WINDOW)
            .map(|echo| (echo.first, echo.last, echo.at_cursor))
        else {
            return Echoed::No;
        };
        // A tab the user wasn't in: the edit is theirs only if it began at its cursor
        // (they switched to it and typed), and not if the file on disk now reads the same
        // (it was reloaded: a pull, a checkout, another program).
        if !active && (!at_cursor || same_as_disk(doc)) {
            return Echoed::Background;
        }
        let near = cursor.line + 1 >= first && cursor.line <= last + 1;
        let Some(echo) = doc.echo.as_mut() else {
            return Echoed::No;
        };
        if !active || near {
            echo.settled.get_or_insert_with(Instant::now);
        } else if let Some(settled) = echo.settled {
            if settled.elapsed() <= BURST {
                return Echoed::OtherPane;
            }
            doc.echo = None; // a real move after typing
        }
        Echoed::User
    }

    /// Whether a report is part of a refresh burst: Zed has every editor re-report its
    /// cursor at once (a language server starting, settings changing), and nobody clicks
    /// two files within 100 ms. The burst's first report may already have switched
    /// documents; that is undone.
    fn in_burst(&mut self, uri: &str, active: bool, now: Instant) -> bool {
        let follows_other = self
            .last_report
            .as_ref()
            .is_some_and(|last| last.uri != uri && now.duration_since(last.at) < BURST);
        if !follows_other && self.burst_until.is_none_or(|until| now >= until) {
            return false;
        }
        self.burst_until = Some(now + BURST);
        if let Some(last) = self.last_report.take()
            && now.duration_since(last.at) < BURST
            && let Some((active, recent)) = last.before
        {
            self.active = active;
            self.recent = recent;
        }
        // The document already in use changes nothing.
        !active
    }

    /// Takes over a diff's record when another Revs process rewrote it (a hover card
    /// link, the command line, or the server of another project folder reopened the diff
    /// in a new tab on this buffer). Its landing applies when the cursor is where it put it.
    fn refresh_view(&mut self, uri: &str, reported: Option<Cursor>) {
        let Some(doc) = self.docs.get_mut(uri) else {
            return;
        };
        let Kind::View(pair) = &doc.kind else {
            return;
        };
        let modified = view::record_modified(&pair.right_path);
        if modified == doc.record_modified {
            return;
        }
        doc.record_modified = modified;
        let Some(record) = Pair::load(&pair.right_path).filter(|record| record != &**pair) else {
            return;
        };
        let cursor = reported.or(doc.cursor);
        match record.landing.filter(|landing| (view::now() - landing.at).abs() <= 30) {
            Some(landing)
                if cursor.is_some_and(|cursor| {
                    cursor.empty && (cursor.line, cursor.character) == (landing.line, landing.character)
                }) =>
            {
                doc.cursor = Some(Cursor {
                    placed: true,
                    ..Cursor::at(landing.line, landing.character)
                });
                doc.focus = Some(landing.focus);
            }
            _ => {
                // Where the old route left the cursor means nothing for the new one.
                if let Some(stored) = &mut doc.cursor {
                    stored.placed = false;
                }
                doc.focus = None;
            }
        }
        doc.kind = Kind::View(Box::new(record));
    }

    /// Makes sure a usable blame exists or is being computed. Returns true if ready.
    fn ensure_blame(&mut self, uri: &str) -> bool {
        let Some(doc) = self.docs.get_mut(uri) else {
            return false;
        };
        let Kind::Working(working) = &mut doc.kind else {
            return false;
        };
        let head = working.repo.head();
        if working.attempted && working.blame_head != head {
            working.stale = true;
        }
        if working.pending || (working.attempted && !working.stale) {
            return working.blame.is_some();
        }
        working.pending = true;
        // A save or HEAD move while this runs sets `stale` again and triggers another run.
        working.stale = false;
        working.edits_during_blame.clear();
        let generation = next_generation();
        doc.generation.store(generation, Ordering::Relaxed);
        let generation_flag = doc.generation.clone();
        let repo = working.repo.clone();
        let rel = working.rel.clone();
        let crlf = working.crlf;
        let text: Arc<str> = if crlf && !doc.text.as_str().contains('\r') {
            Arc::from(doc.text.as_str().replace('\n', "\r\n"))
        } else {
            Arc::from(doc.text.as_str())
        };
        working.snapshot = Some((doc.version, text.clone()));
        let line_count = doc.text.line_count();
        let events = self.events.clone();
        let uri_owned = uri.to_string();

        self.pool.spawn(move || {
            if generation_flag.load(Ordering::Relaxed) != generation {
                // Closed or superseded before we started; let a newer request run.
                let _ = events.send(Event::Blame {
                    uri: uri_owned,
                    generation,
                    head: None,
                    blame: None,
                });
                return;
            }
            let blame = repo.blame(&rel, text.as_bytes(), None, None, line_count).ok();
            let _ = events.send(Event::Blame {
                uri: uri_owned,
                generation,
                head,
                blame,
            });
        });
        let ready = matches!(&doc.kind, Kind::Working(working) if working.blame.is_some());
        self.quick_blame(uri);
        ready
    }

    /// Large files take a while to blame in full; blame just the cursor line meanwhile so
    /// the annotation shows up right away.
    fn quick_blame(&mut self, uri: &str) {
        let Some(doc) = self.docs.get_mut(uri) else {
            return;
        };
        let (Some(line), line_count, version) = (doc.cursor_line(), doc.text.line_count(), doc.version)
        else {
            return;
        };
        let Kind::Working(working) = &mut doc.kind else {
            return;
        };
        if working.blame.is_some()
            || !working.pending
            || working.quick_in_flight
            || line_count <= QUICK_BLAME_LINES
            || working.quick_line == Some(line)
            || line as usize >= line_count
        {
            return;
        }
        // Blame the text as it is now: the cursor line refers to it.
        let text = match &working.snapshot {
            Some((snapshot_version, text)) if *snapshot_version == version => text.clone(),
            _ if working.crlf && !doc.text.as_str().contains('\r') => {
                Arc::from(doc.text.as_str().replace('\n', "\r\n"))
            }
            _ => Arc::from(doc.text.as_str()),
        };
        working.quick_line = Some(line);
        working.quick_in_flight = true;
        let generation = doc.generation.load(Ordering::Relaxed);
        let flag = doc.generation.clone();
        let (repo, rel) = (working.repo.clone(), working.rel.clone());
        let events = self.events.clone();
        let uri = uri.to_string();
        std::thread::spawn(move || {
            let blame = if flag.load(Ordering::Relaxed) == generation {
                repo.blame(&rel, text.as_bytes(), Some((line, line)), None, line_count).ok()
            } else {
                None
            };
            let _ = events.send(Event::QuickBlame {
                uri,
                generation,
                version,
                line,
                blame,
            });
        });
    }

    fn blame_done(
        &mut self,
        uri: &str,
        generation: u64,
        head: Option<String>,
        blame: Option<Blame>,
    ) {
        let Some(doc) = self.docs.get_mut(uri) else {
            return;
        };
        let current = doc.generation.load(Ordering::Relaxed);
        let line_count = doc.text.line_count();
        let Kind::Working(working) = &mut doc.kind else {
            return;
        };
        if current != generation {
            return;
        }
        working.pending = false;
        working.attempted = true;
        let rerun = working.stale;
        working.blame_head = head;
        let edits = std::mem::take(&mut working.edits_during_blame);
        working.blame = blame.and_then(|mut blame| {
            for edit in edits {
                blame.apply_edit(edit);
            }
            (blame.lines.len() == line_count).then_some(blame)
        });
        working.quick = None;
        working.quick_line = None;
        working.snapshot = None;
        self.schedule_relief();
        if self.active.as_deref() == Some(uri) {
            self.refresh_inlays();
        }
        if rerun && self.annotation() {
            // Saved (or HEAD moved) while git was running.
            self.ensure_blame(uri);
        }
    }

    // ---- inlay hints -----------------------------------------------------------------

    fn inlay_hints(&mut self, params: &Value) -> Value {
        let Some(uri) = params_uri(params).map(str::to_string) else {
            return json!([]);
        };
        if !self.annotation() || self.active.as_deref() != Some(uri.as_str()) {
            return json!([]);
        }
        let (start, _) = position(&params["range"]["start"]);
        let (end, end_character) = position(&params["range"]["end"]);
        let Some(line) = self.docs.get(&uri).and_then(Doc::cursor_line) else {
            return json!([]);
        };
        // Zed asks in 50-row chunks whose end is column 0 of the next chunk's first row;
        // treat that end as exclusive so a boundary line isn't annotated twice.
        let in_range = line >= start
            && (line < end || (line == end && (end_character > 0 || end == start)));
        if !in_range {
            return json!([]);
        }
        if !matches!(self.docs.get(&uri).map(|doc| &doc.kind), Some(Kind::Working(_))) {
            return json!([]);
        }
        self.ensure_blame(&uri);
        let Some(doc) = self.docs.get(&uri) else {
            return json!([]);
        };
        let hint = match &doc.kind {
            Kind::Working(working) => self.working_hint(doc, working, line),
            _ => None,
        };
        let version = doc.version;
        let Some(mut hint) = hint else {
            return json!([]);
        };
        // The hint's data is only an id: resolving trusts nothing the client sends back.
        let id = self.next_hint;
        self.next_hint += 1;
        self.hints.insert(id, HintRecord { uri, line, version });
        self.hint_order.push_back(id);
        while self.hint_order.len() > 64 {
            if let Some(old) = self.hint_order.pop_front() {
                self.hints.remove(&old);
            }
        }
        hint["data"] = json!({ "id": id });
        json!([hint])
    }

    fn working_hint(&self, doc: &Doc, working: &Working, line: u32) -> Option<Value> {
        // The empty "line" after a final newline isn't a line git knows about.
        if line as usize + 1 == doc.text.line_count() && doc.text.line(line as usize).is_empty() {
            return None;
        }
        let (entry, commit) = cached_line(working, line)?;
        let uncommitted = entry.commit == EDITED || commit.as_ref().is_none_or(|c| c.is_uncommitted());
        let text = if uncommitted {
            "You • Uncommitted changes".to_string()
        } else {
            let commit = commit.as_ref()?;
            let who = if working.repo.is_you(&commit.author, &commit.mail) {
                "You".to_string()
            } else {
                commit.author.clone()
            };
            format!(
                "{who}, {} • {}",
                format::relative_time(commit.time, format::now()),
                format::truncate(&commit.summary, self.settings.summary_length)
            )
        };
        Some(json!({
            "position": { "line": line, "character": doc.text.utf16_len(line as usize) },
            "label": [{ "value": text }],
            "paddingLeft": true,
        }))
    }

    fn schedule_relief(&mut self) {
        if self.relief_at.is_none() {
            self.relief_at = Some(Instant::now() + Duration::from_millis(400));
        }
    }

    fn refresh_inlays(&mut self) {
        if self.inlay_refresh.in_flight.is_some() {
            self.inlay_refresh.queued = true;
            return;
        }
        let id = self.request("workspace/inlayHint/refresh", None);
        self.inlay_refresh.in_flight = Some(id);
    }

    // ---- tooltips (inlay hint resolve) -----------------------------------------------

    fn cache_tooltip(&mut self, key: String, markdown: Arc<str>) {
        if self.tooltips.insert(key.clone(), (markdown, Instant::now())).is_none() {
            self.tooltip_order.push_back(key);
            while self.tooltip_order.len() > 64 {
                if let Some(old) = self.tooltip_order.pop_front() {
                    self.tooltips.remove(&old);
                }
            }
        }
    }

    fn resolve_hint(&mut self, id: Value, mut hint: Value) {
        // Only hints this server handed out, for a document that hasn't changed since.
        let record = hint["data"]["id"].as_u64().and_then(|hint_id| self.hints.get(&hint_id));
        let job = record
            .filter(|record| self.docs.get(&record.uri).is_some_and(|doc| doc.version == record.version))
            .and_then(|record| self.tooltip_job(&record.uri, record.line));
        let Some(job) = job else {
            self.respond(id, hint);
            return;
        };
        // Cards are rebuilt after an hour, well before their links expire, or sooner when
        // a link's record was cleared to make room.
        if job.cacheable()
            && let Some((markdown, _)) = self
                .tooltips
                .get(&job.key)
                .filter(|(markdown, made)| made.elapsed() < Duration::from_secs(3600) && view::links_alive(markdown))
                .cloned()
        {
            set_tooltip(&mut hint, &markdown);
            self.respond(id, hint);
            return;
        }
        let events = self.events.clone();
        self.pool.spawn(move || {
            let markdown: Arc<str> = Arc::from(job.build());
            set_tooltip(&mut hint, &markdown);
            if job.cacheable() {
                let _ = events.send(Event::Tooltip {
                    key: job.key.clone(),
                    markdown,
                });
            }
            let _ = events.send(Event::Respond { id, result: hint });
        });
    }

    fn tooltip_job(&self, uri: &str, line: u32) -> Option<actions::Tooltip> {
        let doc = self.docs.get(uri)?;
        let Kind::Working(working) = &doc.kind else {
            return None;
        };
        let (entry, commit) = cached_line(working, line)?;
        let target = line_target(entry, commit.as_ref(), line, doc);
        Some(actions::Tooltip::for_line(
            working.repo.clone(),
            self.root.clone(),
            self.single_file.clone(),
            working.rel.clone(),
            target,
        ))
    }

    // ---- commands --------------------------------------------------------------------

    fn execute_command(&mut self, id: Value, params: &Value) {
        let reply = Reply {
            id: Some(id),
            events: self.events.clone(),
        };
        let command = params["command"].as_str().unwrap_or_default().to_string();
        let arguments = params["arguments"].as_array().cloned().unwrap_or_default();
        if command == TOGGLE_BLAME {
            self.toggle_blame();
            reply.done();
            return;
        }
        // From the command picker there are no arguments: use the last cursor position.
        // (Text typed into the picker's Arguments field is ignored unless it names an
        // open file.)
        let explicit = arguments
            .first()
            .and_then(Value::as_str)
            .filter(|uri| uri.starts_with("file://") && self.docs.contains_key(*uri))
            .map(str::to_string);
        let uri = explicit.clone().or_else(|| self.active.clone());
        let Some(uri) = uri.filter(|uri| self.docs.contains_key(uri)) else {
            self.say("Click the line first, then run the command");
            return;
        };
        self.refresh_view(&uri, None);
        let cursor = match arguments.get(1).and_then(Value::as_u64).filter(|_| explicit.is_some()) {
            Some(line) => Some(Cursor {
                placed: true,
                ..Cursor::at(line as u32, 1)
            }),
            None => self.docs.get(&uri).and_then(|doc| doc.cursor),
        };
        let Some(cursor) = cursor else {
            self.say("Click the line first, then run the command");
            return;
        };
        match command.as_str() {
            PREVIOUS => self.navigate(&uri, cursor, false, reply),
            NEXT => self.navigate(&uri, cursor, true, reply),
            OPEN_FILE => self.open_working_file(&uri, cursor, reply),
            _ if command.starts_with(OPEN_COMMIT_PREFIX) => self.open_commit(&uri, cursor, reply),
            _ => reply.done(),
        }
    }

    fn say(&mut self, message: &str) {
        self.handle_event(Event::ShowMessage(message.to_string()));
    }

    fn toggle_blame(&mut self) {
        let on = !self.annotation();
        self.toggle.set(on);
        if on && let Some(uri) = self.active.clone() {
            self.ensure_blame(&uri);
            self.quick_blame(&uri);
        }
        self.refresh_inlays();
    }

    /// A job for the file a document belongs to.
    fn job_for(&mut self, uri: &str, source: String) -> Option<Job> {
        let doc = self.docs.get(uri)?;
        let (repo, file, root) = match &doc.kind {
            Kind::Working(working) => (working.repo.clone(), working.rel.clone(), self.root.clone()),
            Kind::View(pair) => {
                let repo = self
                    .repos
                    .values()
                    .find(|repo| repo.root == pair.repo)
                    .cloned()
                    .or_else(|| Repo::discover(&pair.repo))?;
                (repo, pair.file.clone(), pair.root.clone())
            }
            Kind::Plain => return None,
        };
        let working_path = repo.root.join(&file);
        let working = self
            .paths
            .get(&working_path)
            .and_then(|working_uri| self.docs.get(working_uri))
            .filter(|doc| matches!(doc.kind, Kind::Working(_)))
            .map(|doc| Arc::<[u8]>::from(disk_text(doc).as_bytes()));
        let elsewhere = working.is_none().then_some(working_path);
        Some(Job {
            repo,
            root,
            single_file: self.single_file.clone(),
            file,
            working,
            elsewhere,
            source,
            events: self.events.clone(),
        })
    }

    fn navigate(&mut self, uri: &str, cursor: Cursor, forward: bool, reply: Reply) {
        let source = match self.docs.get(uri).map(|doc| &doc.kind) {
            Some(Kind::Working(working)) if cursor.last_line > cursor.line => {
                format!("from {}", place_range(&working.rel, "lines", cursor.line, cursor.last_line))
            }
            Some(Kind::Working(working)) => format!("from {}", place(&working.rel, "line", cursor.line)),
            _ => String::new(),
        };
        let Some(mut job) = self.job_for(uri, source) else {
            self.say("Revs works on files in a git repository");
            return;
        };
        let card = self.settings.step_card;
        let Some(doc) = self.docs.get(uri) else {
            return;
        };
        match &doc.kind {
            Kind::Working(working) => {
                if forward {
                    self.say("This is the working file: there's nothing newer");
                    return;
                }
                let known = fresh_line(working, cursor.line).and_then(|(entry, commit)| {
                    let commit = commit?;
                    (entry.commit != EDITED).then_some((commit, entry.orig))
                });
                let text = job.working.clone().unwrap_or_else(|| Arc::from(disk_text(doc).as_bytes()));
                self.commands.spawn(move || {
                    let blobs = Blobs::new(&job.repo);
                    let outcome = if cursor.last_line > cursor.line {
                        history::previous_section_in_file(&blobs, &job.file, &text, cursor.line, cursor.last_line)
                    } else {
                        history::previous_in_file(&blobs, &job.file, &text, cursor.line, known)
                    };
                    job.finish(&blobs, outcome, None, card, reply);
                });
            }
            Kind::View(pair) => {
                let pair = (**pair).clone();
                let focus = doc.focus;
                let command = if forward { "next" } else { "previous" };
                self.commands.spawn(move || {
                    job.fetch_working();
                    let blobs = Blobs::new(&job.repo);
                    // This diff, for the opposite step to return to.
                    let here = |line_focus: Option<Focus>| Back {
                        left: pair.left.clone(),
                        right: pair.right.clone(),
                        line: cursor.line,
                        focus: line_focus.unwrap_or(Focus::Right(cursor.line)),
                        command: command.into(),
                    };
                    // Straight back: the opposite of the step that opened this diff, while
                    // the cursor is still where that step put it.
                    let opposite = if forward { "previous" } else { "next" };
                    if cursor.placed
                        && let Some(back) = pair.back.as_ref().filter(|back| back.command == opposite)
                    {
                        job.source = format!("back to {}", place(&back.right.path, "line", back.line));
                        let outcome = Outcome::Open {
                            left: back.left.clone(),
                            right: back.right.clone(),
                            line: back.line,
                            focus: back.focus,
                            note: None,
                        };
                        job.finish(&blobs, Ok(outcome), Some(here(focus)), card, reply);
                        return;
                    }
                    match navigate_view(&job, &blobs, &pair, cursor, focus, forward) {
                        Ok((outcome, source, step_focus)) => {
                            job.source = source;
                            job.finish(&blobs, Ok(outcome), Some(here(step_focus)), card, reply);
                        }
                        Err(error) => job.finish(&blobs, Err(error), None, card, reply),
                    }
                });
            }
            Kind::Plain => self.say("Revs works on files in a git repository"),
        }
    }

    fn open_working_file(&mut self, uri: &str, cursor: Cursor, reply: Reply) {
        let Some(doc) = self.docs.get(uri) else {
            return;
        };
        let Kind::View(pair) = &doc.kind else {
            self.say("This is already the working file");
            return;
        };
        let pair = (**pair).clone();
        let focus = doc.focus.filter(|_| cursor.placed);
        let Some(mut job) = self.job_for(uri, String::new()) else {
            return;
        };
        self.commands.spawn(move || {
            job.fetch_working();
            let blobs = Blobs::new(&job.repo);
            let working_path = pair.working_path();
            let working = match &job.working {
                Some(text) => Some(text.to_vec()),
                None => std::fs::read(&working_path).ok(),
            };
            let Some(working) = working else {
                job.say(format!("{} no longer exists", pair.file));
                return;
            };
            // A removed line: open the working file where it was removed.
            let (side, line) = match focus {
                Some(Focus::Left(line)) => (&pair.left, line),
                _ => (&pair.right, cursor.line),
            };
            // A working-tree side shows the text as it was when the diff opened.
            let side_text: std::rc::Rc<[u8]> = if side.rev == view::WORKING {
                std::fs::read(&pair.right_path).unwrap_or_default().into()
            } else {
                view::contents(&blobs, side, None)
            };
            let (line, note) = history::working_line(&blobs, side, &side_text, line, &working);
            let _ = job.events.send(Event::ShowDocument {
                uri: format::file_uri(&working_path),
                line: Some(line),
                character: view::indentation(&working, line),
                external: false,
            });
            if let Some(note) = note {
                job.say(capitalize(&note));
            }
            reply.done();
        });
    }

    fn open_commit(&mut self, uri: &str, cursor: Cursor, reply: Reply) {
        let Some(job) = self.job_for(uri, String::new()) else {
            self.say("Revs works on files in a git repository");
            return;
        };
        let Some(doc) = self.docs.get(uri) else {
            return;
        };
        // The commit: the one a diff shows, or the one that last changed the line.
        let sha = match &doc.kind {
            Kind::View(pair) if pair.right.is_commit() => Some(Ok(pair.right.rev.clone())),
            Kind::View(_) => None,
            Kind::Working(working) => fresh_line(working, cursor.line)
                .and_then(|(entry, commit)| (entry.commit != EDITED).then_some(commit?))
                .map(|commit| Ok(commit.sha)),
            Kind::Plain => return,
        };
        let text = match &doc.kind {
            Kind::Working(_) => Some(Arc::<[u8]>::from(disk_text(doc).as_bytes())),
            _ => None,
        };
        self.commands.spawn(move || {
            let sha = match sha {
                Some(sha) => sha,
                None => match text {
                    Some(text) => job
                        .repo
                        .blame(&job.file, &text, Some((cursor.line, cursor.line)), None, cursor.line as usize + 1)
                        .and_then(|blame| {
                            blame
                                .line(cursor.line as usize)
                                .and_then(|(_, commit)| commit.map(|commit| commit.sha.clone()))
                                .ok_or_else(|| io::Error::other("no blame for this line"))
                        }),
                    None => Err(io::Error::other("These are uncommitted changes")),
                },
            };
            match sha {
                Ok(sha) if crate::blame::is_zero_sha(&sha) => job.say("This line isn't committed yet"),
                Ok(sha) => match job.repo.remote() {
                    Some(remote) => {
                        let _ = job.events.send(Event::ShowDocument {
                            uri: remote.commit_url(&sha),
                            line: None,
                            character: 0,
                            external: true,
                        });
                    }
                    None => job.say("This repository has no remote to open the commit on"),
                },
                Err(error) => job.say(explain(&error)),
            }
            reply.done();
        });
    }

    // ---- output ----------------------------------------------------------------------

    fn send(&mut self, message: &Value) {
        if message.get("method").is_some() {
            self.log("->", message);
        }
        let _ = rpc::write_message(&mut self.out, message);
    }

    fn respond(&mut self, id: Value, result: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn respond_error(&mut self, id: Value, code: i64, message: &str) {
        self.send(&json!({
            "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message },
        }));
    }

    fn request(&mut self, method: &str, params: Option<Value>) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        let mut message = json!({ "jsonrpc": "2.0", "id": id, "method": method });
        if let Some(params) = params {
            message["params"] = params;
        }
        self.send(&message);
        id
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }
}

/// The cached blame of a line (full blame, or the quick single-line one).
fn cached_line(working: &Working, line: u32) -> Option<(LineBlame, Option<Commit>)> {
    let blame = match (&working.blame, &working.quick) {
        (Some(blame), _) => blame,
        (None, Some((quick_line, blame))) if *quick_line == line => blame,
        _ => return None,
    };
    let (entry, commit) = blame.line(line as usize)?;
    Some((entry, commit.cloned()))
}

/// The cached blame of a line, if it still describes the repository as it is: complete,
/// not being refreshed, and blamed against the current HEAD.
fn fresh_line(working: &Working, line: u32) -> Option<(LineBlame, Option<Commit>)> {
    let fresh = working.blame.is_some()
        && working.attempted
        && !working.pending
        && !working.stale
        && working.blame_head == working.repo.head();
    if fresh { cached_line(working, line) } else { None }
}

fn line_target(entry: LineBlame, commit: Option<&Commit>, line: u32, doc: &Doc) -> actions::LineTarget {
    match commit {
        Some(commit) if entry.commit != EDITED && !commit.is_uncommitted() => {
            actions::LineTarget::Commit {
                sha: commit.sha.clone(),
                path: commit.filename.clone(),
                previous: commit.previous.clone(),
                orig: entry.orig,
                author: commit.author.clone(),
                mail: commit.mail.clone(),
                time: commit.time,
                summary: commit.summary.clone(),
            }
        }
        _ => actions::LineTarget::Uncommitted {
            line,
            text: doc.text.line(line as usize).trim_end_matches('\r').to_string(),
            buffer: Arc::from(doc.text.as_str()),
        },
    }
}

fn set_tooltip(hint: &mut Value, markdown: &str) {
    let tooltip = json!({ "kind": "markdown", "value": markdown });
    let part = hint
        .get_mut("label")
        .and_then(Value::as_array_mut)
        .and_then(|parts| parts.first_mut())
        .and_then(Value::as_object_mut);
    if let Some(part) = part {
        part.insert("tooltip".into(), tooltip);
    } else if let Some(hint) = hint.as_object_mut() {
        hint.insert("tooltip".into(), tooltip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(old_start: u32, old_count: u32, new_start: u32, new_count: u32) -> diff::Hunk {
        diff::Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
            ..diff::Hunk::default()
        }
    }

    fn at(line: u32, character: u32) -> Cursor {
        Cursor::at(line, character)
    }

    #[test]
    fn classifies_rows_like_zeds_split_diff() {
        // Left 68-69 removed (`@@ -68,2 +67,0 @@`); right
        // 71-74 added after "try {" (left 72, right 70: `@@ -72,0 +71,4 @@`).
        let hunks = vec![hunk(68, 2, 67, 0), hunk(72, 0, 71, 4)];
        let text = b"x\n".repeat(200);
        // A click on a removed line arrives as column 0 of the next right line (68).
        assert_eq!(
            classify(&hunks, at(67, 0), &text),
            Target::Removed { first: 67, last: 68 }
        );
        // The same right line with the cursor further in is the right line itself.
        assert_eq!(
            classify(&hunks, at(67, 8), &text),
            Target::Unchanged { left: 69, right: 67 }
        );
        assert_eq!(classify(&hunks, at(70, 4), &text), Target::Added { right: 70 });
        assert_eq!(classify(&hunks, at(73, 0), &text), Target::Added { right: 73 });
        assert_eq!(
            classify(&hunks, at(75, 8), &text),
            Target::Unchanged { left: 73, right: 75 }
        );
        // A diff Revs opened puts the cursor there itself: never "removed".
        let placed = Cursor {
            placed: true,
            ..at(67, 0)
        };
        assert_eq!(
            classify(&hunks, placed, &text),
            Target::Unchanged { left: 69, right: 67 }
        );
    }

    #[test]
    fn classifies_changed_blocks_and_selections() {
        // Left 10-12 (three lines) became right 10-11 (two lines).
        let hunks = vec![hunk(10, 3, 10, 2)];
        let text = b"x\n".repeat(40);
        assert_eq!(classify(&hunks, at(9, 3), &text), Target::Paired { left: 9, right: 9 });
        assert_eq!(classify(&hunks, at(10, 0), &text), Target::Paired { left: 10, right: 10 });
        assert_eq!(classify(&hunks, at(11, 0), &text), Target::Removed { first: 11, last: 11 });
        assert_eq!(classify(&hunks, at(11, 2), &text), Target::Unchanged { left: 12, right: 11 });
        let selection = Cursor {
            empty: false,
            ..at(11, 0)
        };
        assert_eq!(classify(&hunks, selection, &text), Target::Unchanged { left: 12, right: 11 });
        let section = Cursor {
            last_line: 14,
            empty: false,
            ..at(9, 0)
        };
        assert_eq!(classify(&hunks, section, &text), Target::Section { first: 9, last: 14 });
    }

    #[test]
    fn classifies_removals_at_the_end() {
        // With a final newline a click on rows removed at the end lands on the empty row
        // after the last line.
        let old = "one\ntwo\nthree\nfour\nfive\n";
        let new = "one\ntwo\nthree\n";
        let hunks = diff::hunks(old, new);
        assert_eq!(classify(&hunks, at(3, 0), new.as_bytes()), Target::Removed { first: 3, last: 4 });
        assert_eq!(classify(&hunks, at(2, 2), new.as_bytes()), Target::Unchanged { left: 2, right: 2 });
        // Without one, the last line differs too ("x\n" against "x"), and the click lands
        // at the end of that line.
        let old = "start\nvalue = compute(a)\nlog(value)\n";
        let new = "start\nvalue = compute(a, b)";
        let hunks = diff::hunks(old, new);
        let end = utf16_len("value = compute(a, b)");
        assert_eq!(
            classify(&hunks, at(1, end), new.as_bytes()),
            Target::EndOfLast { right: 1, left: Some(1), first: 2, last: 2 }
        );
        assert_eq!(classify(&hunks, at(1, 5), new.as_bytes()), Target::Paired { left: 1, right: 1 });
        let placed = Cursor {
            placed: true,
            ..at(1, end)
        };
        assert_eq!(classify(&hunks, placed, new.as_bytes()), Target::Paired { left: 1, right: 1 });
    }

    #[test]
    fn plain_columns_only() {
        assert!(plain_columns("    return x;"));
        assert!(!plain_columns("\treturn x;"));
        assert!(!plain_columns("let price = \"€\";"));
    }

    #[test]
    fn places() {
        assert_eq!(place("src/syntax/parser.rs", "line", 213), "parser.rs line 214");
        assert_eq!(place("README.md", "removed line", 0), "README.md removed line 1");
        assert_eq!(place_range("a/b.rs", "lines", 2, 8), "b.rs lines 3–9");
    }

    #[test]
    fn indentation_columns() {
        assert_eq!(view::indentation(b"a\n    b\n\tc\n", 1), 4);
        assert_eq!(view::indentation(b"a\n    b\n\tc\n", 2), 1);
        assert_eq!(view::indentation(b"a\n", 0), 0);
        assert_eq!(view::indentation(b"a\n", 7), 0);
        assert_eq!(view::indentation(b"  \r\n", 0), 2);
    }

    #[test]
    fn commands_sort_in_menu_order() {
        let open_commit = format!("{OPEN_COMMIT_PREFIX}GitHub");
        let order = [PREVIOUS, TOGGLE_BLAME, NEXT, OPEN_FILE, open_commit.as_str()];
        let mut sorted = order;
        sorted.sort();
        assert_eq!(sorted, order);
        // They read as before.
        let shown: Vec<String> = order.iter().map(|name| name.replace('\u{200B}', "")).collect();
        assert_eq!(
            shown,
            [
                "Revs: Previous Revision",
                "Revs: Toggle Line Blame",
                "Revs: Next Revision",
                "Revs: Open Working File",
                "Revs: Open Commit on GitHub",
            ]
        );
    }

    #[test]
    fn explains_git_errors() {
        assert_eq!(
            explain(&io::Error::other("git blame: no such path 'x' in HEAD")),
            "This file isn't committed yet"
        );
        assert_eq!(explain(&io::Error::other("other")), "other");
    }
}
