//! Git access: repository discovery from the filesystem, and short-lived `git` processes
//! for everything else (blame, log, blobs, diffs).

use crate::blame::Blame;
use crate::diff::Hunk;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Blame with the diff algorithm Zed draws its diffs with, so the lines git attributes
/// are the lines Zed shows as changed.
const HISTOGRAM: &str = "--diff-algorithm=histogram";

pub struct Repo {
    /// Working tree root (`git rev-parse --show-toplevel`).
    pub root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
    head: Mutex<(Instant, Option<String>)>,
    remote: OnceLock<Option<Remote>>,
    identity: OnceLock<Identity>,
}

#[derive(Default)]
pub struct Identity {
    pub name: String,
    pub email: String,
}

/// The git executable. `/usr/bin/git` is an `xcrun` shim that roughly doubles the cost of
/// every call, so the real binary behind it (`$(git --exec-path)/git`) is used when present.
fn git_binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        let output = Command::new("git")
            .arg("--exec-path")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        if let Ok(output) = output
            && output.status.success()
        {
            let candidate = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()).join("git");
            use std::os::unix::fs::PermissionsExt as _;
            if fs::metadata(&candidate).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0) {
                return candidate;
            }
        }
        PathBuf::from("git")
    })
}

/// Reads a small file of a repository's own records (`.git`, HEAD, refs): only a regular
/// file, and at most `limit` bytes. A folder can hold anything under those names (a FIFO
/// would block, a link to /dev/zero never ends).
fn read_small(path: &Path, limit: u64) -> Option<String> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().is_ok_and(|meta| meta.is_file()) {
        return None;
    }
    let mut text = String::new();
    file.take(limit).read_to_string(&mut text).ok()?;
    Some(text)
}

/// A full object id (SHA-1 or SHA-256).
fn is_object_id(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl Repo {
    /// Finds the repository containing `path` by walking up to the nearest `.git`.
    pub fn discover(path: &Path) -> Option<Arc<Repo>> {
        let mut dir = if path.is_dir() {
            path
        } else {
            path.parent()?
        };
        loop {
            let dot_git = dir.join(".git");
            if let Ok(meta) = fs::metadata(&dot_git) {
                let git_dir = if meta.is_dir() {
                    dot_git
                } else {
                    // Linked worktree or submodule: "gitdir: <path>".
                    let content = read_small(&dot_git, 4096)?;
                    let target = content.trim().strip_prefix("gitdir:")?.trim();
                    let target = Path::new(target);
                    if target.is_absolute() {
                        target.to_path_buf()
                    } else {
                        dir.join(target)
                    }
                };
                let common_dir = match read_small(&git_dir.join("commondir"), 4096) {
                    Some(common) => {
                        let common = Path::new(common.trim());
                        if common.is_absolute() {
                            common.to_path_buf()
                        } else {
                            git_dir.join(common)
                        }
                    }
                    None => git_dir.clone(),
                };
                return Some(Arc::new(Repo {
                    root: dir.to_path_buf(),
                    git_dir,
                    common_dir,
                    head: Mutex::new((Instant::now() - Duration::from_secs(60), None)),
                    remote: OnceLock::new(),
                    identity: OnceLock::new(),
                }));
            }
            dir = dir.parent()?;
        }
    }

    /// A `git` command running in the repository root. Like Zed's own git calls, it never
    /// runs a repository's fsmonitor hook, and never runs gpg to show signatures (whose
    /// output would also corrupt the formats Revs parses).
    pub fn git(&self) -> Command {
        let mut command = Command::new(git_binary());
        command
            .current_dir(&self.root)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .args(["-c", "core.fsmonitor=false", "-c", "log.showSignature=false"])
            .args(["-c", "core.quotepath=off", "--no-pager"])
            .stdin(Stdio::null());
        command
    }

    /// A working-tree file by its path in the repository, when that is a plain relative
    /// path (one from history can say `../x`) to a regular file, with no symlinked folder
    /// on the way (git never checks a file out beneath a symlink). The file itself may be a
    /// tracked symlink, which Zed follows too.
    pub fn working_file(&self, path: &str) -> Option<PathBuf> {
        let relative = Path::new(path);
        let parts: Vec<_> = relative.components().collect();
        if path.is_empty() || !parts.iter().all(|part| matches!(part, std::path::Component::Normal(_))) {
            return None;
        }
        let mut folder = self.root.clone();
        for part in &parts[..parts.len() - 1] {
            folder.push(part);
            if !fs::symlink_metadata(&folder).is_ok_and(|meta| meta.is_dir()) {
                return None;
            }
        }
        let file = self.root.join(relative);
        fs::metadata(&file).is_ok_and(|meta| meta.is_file()).then_some(file)
    }

    /// The text of a working-tree file (see [`Repo::working_file`]).
    pub fn read_working(&self, path: &str) -> io::Result<Vec<u8>> {
        use std::io::Read as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let file = self
            .working_file(path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no file {path} in the working tree")))?;
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(file)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("{path} isn't a file")));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// HEAD's commit id, read from the ref files (re-read at most every 300 ms).
    pub fn head(&self) -> Option<String> {
        let mut cached = self.head.lock().unwrap();
        if cached.0.elapsed() < Duration::from_millis(300) {
            return cached.1.clone();
        }
        let head = self.read_head();
        *cached = (Instant::now(), head.clone());
        head
    }

    fn read_head(&self) -> Option<String> {
        self.read_head_files().or_else(|| {
            // Ref storage other than files (reftable keeps HEAD as "refs/heads/.invalid").
            let output = run(self.git().args(["rev-parse", "--verify", "-q", "--end-of-options", "HEAD"])).ok()?;
            let oid = String::from_utf8_lossy(&output).trim().to_string();
            is_object_id(&oid).then_some(oid)
        })
    }

    /// HEAD from the ref files git keeps by default.
    fn read_head_files(&self) -> Option<String> {
        let head = read_small(&self.git_dir.join("HEAD"), 4096)?;
        let head = head.trim();
        let Some(reference) = head.strip_prefix("ref:").map(str::trim) else {
            return is_object_id(head).then(|| head.to_string());
        };
        // A branch name, never a path out of the repository's records.
        if !reference.starts_with("refs/") || reference.split('/').any(|part| part.is_empty() || part.starts_with('.')) {
            return None;
        }
        for dir in [&self.git_dir, &self.common_dir] {
            if let Some(oid) = read_small(&dir.join(reference), 4096) {
                let oid = oid.trim();
                return is_object_id(oid).then(|| oid.to_string());
            }
        }
        let packed = read_small(&self.common_dir.join("packed-refs"), 256 << 20)?;
        packed.lines().find_map(|line| {
            let (oid, name) = line.split_once(' ')?;
            (name == reference && is_object_id(oid)).then(|| oid.to_string())
        })
    }

    pub fn identity(&self) -> &Identity {
        self.identity.get_or_init(|| Identity {
            name: self.config("user.name").unwrap_or_default(),
            email: self.config("user.email").unwrap_or_default().to_lowercase(),
        })
    }

    pub fn is_you(&self, author: &str, mail: &str) -> bool {
        let identity = self.identity();
        (!identity.email.is_empty() && mail.eq_ignore_ascii_case(&identity.email))
            || (!identity.name.is_empty() && author == identity.name)
    }

    pub fn remote(&self) -> Option<&Remote> {
        self.remote
            .get_or_init(|| {
                // `get-url` applies `insteadOf` rewrites, like git itself.
                let get_url = |name: &str| {
                    run(self.git().args(["remote", "get-url", "--end-of-options", name]))
                        .ok()
                        .map(|url| String::from_utf8_lossy(&url).trim().to_string())
                        .filter(|url| !url.is_empty())
                };
                let url = get_url("origin").or_else(|| {
                    let remotes = run(self.git().arg("remote")).ok()?;
                    let first = String::from_utf8_lossy(&remotes).lines().next()?.to_string();
                    get_url(&first)
                })?;
                Remote::parse(&url)
            })
            .as_ref()
    }

    fn config(&self, key: &str) -> Option<String> {
        let output = run(self.git().args(["config", "--get", key])).ok()?;
        let value = String::from_utf8_lossy(&output).trim().to_string();
        (!value.is_empty()).then_some(value)
    }

    /// Path of `file` relative to the repository root, with `/` separators.
    pub fn relative(&self, file: &Path) -> Option<String> {
        let relative = file.strip_prefix(&self.root).ok()?;
        Some(relative.to_string_lossy().replace('\\', "/"))
    }

    /// Blames `contents` (the buffer text) as `path`. `range` limits it to 0-based lines
    /// `first..=last`; otherwise the whole text (`line_count` lines) is blamed. `base`
    /// pins the commit the text is compared with (default: HEAD). The output is parsed
    /// while git streams it, so it's never held in memory whole.
    pub fn blame(
        &self,
        path: &str,
        contents: &[u8],
        range: Option<(u32, u32)>,
        base: Option<&str>,
        line_count: usize,
    ) -> io::Result<Blame> {
        let mut command = self.git();
        command.args(["blame", "--incremental", "--no-textconv", HISTOGRAM, "--contents", "-"]);
        let spec;
        if let Some((first, last)) = range {
            spec = format!("{},{}", first + 1, last + 1);
            command.args(["-L", &spec]);
        }
        // Not `--end-of-options`: with `--` git blame then misreads the path as a
        // revision. `base` is always a validated full hex id.
        if let Some(base) = base {
            command.arg(base);
        }
        command.args(["--", path]);
        let (first, count) = match range {
            Some((first, last)) => (first, (last - first + 1) as usize),
            None => (0, line_count),
        };
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        std::thread::scope(|scope| {
            // Feed the text on its own thread so neither pipe can fill up and stall git.
            scope.spawn(move || {
                let _ = stdin.write_all(contents);
            });
            let errors = scope.spawn(move || read_error(stderr));
            let blame = Blame::parse(io::BufReader::new(stdout), first, count);
            let status = child.wait()?;
            let message = errors.join().unwrap_or_default();
            if status.success() {
                Ok(blame)
            } else {
                Err(git_error("git blame", status, &message))
            }
        })
    }

    /// Blames 0-based lines `first..=last` of `path` as of commit `rev`.
    pub fn blame_rev(&self, rev: &str, path: &str, first: u32, last: u32) -> io::Result<Blame> {
        let spec = format!("{},{}", first + 1, last + 1);
        // `--end-of-options` must not be combined with `--` here: git blame then misreads
        // the path as a revision. `rev` is always a validated full hex id.
        let output = self
            .git()
            .args(["blame", "--incremental", "--no-textconv", HISTOGRAM, "-L", &spec, rev, "--", path])
            .stderr(Stdio::piped())
            .output()?;
        if !output.status.success() {
            return Err(git_error(
                "git blame",
                output.status,
                &String::from_utf8_lossy(&output.stderr),
            ));
        }
        Ok(Blame::parse(
            output.stdout.as_slice(),
            first,
            (last - first + 1) as usize,
        ))
    }

    /// Follows 0-based line `line` of `path` at `start` forward to `end`: the last revision
    /// where the line still exists unchanged (`commit.sha`, with the line's 0-based number
    /// there), and in `commit.previous` the child that changed or removed it.
    pub fn reverse_blame(
        &self,
        start: &str,
        end: &str,
        path: &str,
        line: u32,
    ) -> io::Result<(crate::blame::Commit, u32)> {
        let blame = self.reverse_blame_lines(start, end, path, line, line)?;
        let (entry, commit) = blame
            .line(line as usize)
            .ok_or_else(|| io::Error::other("git blame --reverse gave no answer"))?;
        let commit = commit
            .cloned()
            .ok_or_else(|| io::Error::other("git blame --reverse gave no answer"))?;
        Ok((commit, entry.orig))
    }

    /// [`Repo::reverse_blame`] for 0-based lines `first..=last`.
    pub fn reverse_blame_lines(&self, start: &str, end: &str, path: &str, first: u32, last: u32) -> io::Result<Blame> {
        let spec = format!("{},{}", first + 1, last + 1);
        let range = format!("{start}..{end}");
        let mut last_error = None;
        // First-parent keeps main-line changes from being credited to merges; it refuses
        // ranges that leave the first-parent chain, so fall back to a plain walk then.
        for first_parent in [true, false] {
            let mut command = self.git();
            command.args(["blame", "--incremental", "--no-textconv", HISTOGRAM, "--reverse"]);
            if first_parent {
                command.arg("--first-parent");
            }
            let output = command
                .args(["-L", &spec, &range, "--", path])
                .stderr(Stdio::piped())
                .output()?;
            if !output.status.success() {
                last_error = Some(git_error(
                    "git blame --reverse",
                    output.status,
                    &String::from_utf8_lossy(&output.stderr),
                ));
                continue;
            }
            return Ok(Blame::parse(output.stdout.as_slice(), first, (last - first + 1) as usize));
        }
        Err(last_error.unwrap_or_else(|| io::Error::other("git blame --reverse failed")))
    }

    /// The first commit after `from` (on the way to `to`) that touched `path`.
    pub fn next_commit_touching(&self, from: &str, to: &str, path: &str) -> Option<String> {
        let range = format!("{from}..{to}");
        let output = run(self.git().args([
            "rev-list",
            "--reverse",
            "--ancestry-path",
            "--end-of-options",
            &range,
            "--",
            path,
        ]))
        .ok()?;
        String::from_utf8_lossy(&output)
            .split_whitespace()
            .next()
            .map(str::to_string)
    }

    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        self.git()
            .args(["merge-base", "--is-ancestor", "--end-of-options", ancestor, descendant])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// File contents at a revision (`None` when the path doesn't exist there).
    pub fn show(&self, rev: &str, path: &str) -> Option<Vec<u8>> {
        run(self
            .git()
            .args(["cat-file", "blob", "--end-of-options", &format!("{rev}:{path}")]))
        .ok()
    }

    /// Full commit message.
    pub fn message(&self, sha: &str) -> Option<String> {
        let output = run(self
            .git()
            .args(["show", "-s", "--format=%B", "--end-of-options", sha]))
        .ok()?;
        Some(String::from_utf8_lossy(&output).trim_end().to_string())
    }

    /// Unified diff text between two blobs (`rev:path`), zero context.
    pub fn diff_blobs(&self, old: &str, new: &str) -> Option<String> {
        let output = self
            .git()
            .args([
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--histogram",
                "-U0",
                "--end-of-options",
                old,
                new,
            ])
            .stderr(Stdio::null())
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Author, email, time and subject of a commit.
    pub fn describe(&self, rev: &str) -> Option<(String, String, i64, String)> {
        let output = run(self.git().args([
            "log",
            "-1",
            "--format=%an%x1f%ae%x1f%at%x1f%s",
            "--end-of-options",
            rev,
        ]))
        .ok()?;
        let text = String::from_utf8_lossy(&output);
        let fields: Vec<&str> = text.trim_end_matches('\n').split('\x1f').collect();
        (fields.len() >= 4).then(|| {
            (
                fields[0].to_string(),
                fields[1].to_string(),
                fields[2].parse().unwrap_or(0),
                fields[3..].join("\x1f"),
            )
        })
    }
}

fn read_error(mut stderr: impl io::Read) -> String {
    let mut text = String::new();
    let _ = stderr.read_to_string(&mut text);
    text
}

/// A git failure with git's own first line of explanation (e.g. a missing
/// `blame.ignoreRevsFile`), so the user can act on it.
fn git_error(what: &str, status: std::process::ExitStatus, stderr: &str) -> io::Error {
    let reason = stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.trim_start_matches("fatal: ").trim_start_matches("error: "));
    match reason {
        Some(reason) => io::Error::other(format!("{what}: {reason}")),
        None => io::Error::other(format!("{what} exited with {status}")),
    }
}

/// File contents at revisions, fetched once per command.
/// A blob fetched for a step: revision, path, contents.
type Blob = (String, String, std::rc::Rc<[u8]>);

pub struct Blobs<'a> {
    repo: &'a Repo,
    cache: std::cell::RefCell<Vec<Blob>>,
}

impl<'a> Blobs<'a> {
    pub fn new(repo: &'a Repo) -> Blobs<'a> {
        Blobs {
            repo,
            cache: std::cell::RefCell::new(Vec::new()),
        }
    }

    pub fn repo(&self) -> &'a Repo {
        self.repo
    }

    /// `rev:path` (empty when the path doesn't exist there).
    pub fn get(&self, rev: &str, path: &str) -> std::rc::Rc<[u8]> {
        if let Some((_, _, bytes)) = self
            .cache
            .borrow()
            .iter()
            .find(|(known_rev, known_path, _)| known_rev == rev && known_path == path)
        {
            return bytes.clone();
        }
        let bytes: std::rc::Rc<[u8]> = self.repo.show(rev, path).unwrap_or_default().into();
        let mut cache = self.cache.borrow_mut();
        if cache.len() >= 6 {
            cache.remove(0);
        }
        cache.push((rev.to_string(), path.to_string(), bytes.clone()));
        bytes
    }

    /// Whether `rev:path` exists.
    pub fn exists(&self, rev: &str, path: &str) -> bool {
        !self.get(rev, path).is_empty() || self.repo.show(rev, path).is_some()
    }
}

/// When a few commits were made — (id, committer time, author time) — in one git call.
pub fn commit_times(repo: &Repo, shas: &[String]) -> Vec<(String, i64, i64)> {
    if shas.is_empty() {
        return Vec::new();
    }
    let mut command = repo.git();
    command.args(["log", "--no-walk=unsorted", "--format=%H %ct %at", "--end-of-options"]);
    command.args(shas.iter().filter(|sha| is_commit_id(sha)));
    let Ok(output) = run(&mut command) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split(' ');
            Some((parts.next()?.to_string(), parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
        })
        .collect()
}

/// A commit id as Revs hands it around: 4–64 hex digits. Anything else (e.g. from a
/// crafted `revs://` link) is rejected before it can reach git.
pub fn is_commit_id(text: &str) -> bool {
    (4..=64).contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Hunk headers (and removed/added lines) of a `git diff -U0`.
pub fn parse_hunks(diff: &str) -> Vec<Hunk> {
    fn range(spec: &str) -> Option<(u32, u32)> {
        let spec = &spec[1..];
        match spec.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((spec.parse().ok()?, 1)),
        }
    }
    let mut hunks: Vec<Hunk> = Vec::new();
    // Lines still expected for the current hunk: removed, then added.
    let (mut removed, mut added) = (0u32, 0u32);
    for line in diff.lines() {
        if removed > 0 && let Some(text) = line.strip_prefix('-') {
            removed -= 1;
            if let Some(hunk) = hunks.last_mut() {
                hunk.old_lines.push(text.trim_end_matches('\r').to_string());
            }
            continue;
        }
        if added > 0 && let Some(text) = line.strip_prefix('+') {
            added -= 1;
            if let Some(hunk) = hunks.last_mut() {
                hunk.new_lines.push(text.trim_end_matches('\r').to_string());
            }
            continue;
        }
        let Some(rest) = line.strip_prefix("@@ ") else {
            continue;
        };
        let mut parts = rest.split(' ');
        let (Some(old), Some(new)) = (parts.next(), parts.next()) else {
            continue;
        };
        let (Some((old_start, old_count)), Some((new_start, new_count))) = (range(old), range(new))
        else {
            continue;
        };
        (removed, added) = (old_count, new_count);
        hunks.push(Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
            old_lines: Vec::with_capacity(old_count.min(1024) as usize),
            new_lines: Vec::with_capacity(new_count.min(1024) as usize),
        });
    }
    hunks
}

pub fn run(command: &mut Command) -> io::Result<Vec<u8>> {
    let output = command.stderr(Stdio::null()).output()?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(io::Error::other(format!("git exited with {}", output.status)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    /// Web URL of the repository, e.g. `https://github.com/owner/repo`.
    pub web: String,
    pub host: Host,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Host {
    GitHub,
    GitLab,
    Bitbucket,
    Other,
}

impl Remote {
    pub fn parse(url: &str) -> Option<Remote> {
        let url = url.trim();
        // Local remotes have no web page.
        if url.starts_with("file:") || url.starts_with('/') || url.starts_with('.') || url.starts_with('~') {
            return None;
        }
        let (host, path) = if let Some(rest) = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .or_else(|| url.strip_prefix("ssh://"))
            .or_else(|| url.strip_prefix("git://"))
            .or_else(|| url.strip_prefix("git+ssh://"))
        {
            let (authority, path) = rest.split_once('/')?;
            let host = authority.rsplit('@').next()?;
            let host = host.split(':').next()?;
            (host.to_string(), path.to_string())
        } else {
            // scp-like: [user@]host:owner/repo.git
            let (authority, path) = url.split_once(':')?;
            let host = authority.rsplit('@').next()?;
            (host.to_string(), path.to_string())
        };
        let path = path.trim_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        // A web page needs a real host name (`gh:` style aliases aren't one).
        let host_ok = host.contains('.') && host.bytes().all(|byte| byte.is_ascii_alphanumeric() || b".-_".contains(&byte));
        if host.is_empty() || path.is_empty() || !host_ok {
            return None;
        }
        // The URL goes into Markdown links: anything else in the path is percent-encoded,
        // so it can't end a link or start another.
        let path: String = path
            .bytes()
            .map(|byte| match byte {
                byte if byte.is_ascii_alphanumeric() || b"-._~/%+@".contains(&byte) => char::from(byte).to_string(),
                byte => format!("%{byte:02X}"),
            })
            .collect();
        let lower = host.to_lowercase();
        let host_kind = if lower.contains("github") {
            Host::GitHub
        } else if lower.contains("gitlab") {
            Host::GitLab
        } else if lower.contains("bitbucket") {
            Host::Bitbucket
        } else {
            Host::Other
        };
        Some(Remote {
            web: format!("https://{host}/{path}"),
            host: host_kind,
        })
    }

    pub fn name(&self) -> &'static str {
        match self.host {
            Host::GitHub => "GitHub",
            Host::GitLab => "GitLab",
            Host::Bitbucket => "Bitbucket",
            Host::Other => "Remote",
        }
    }

    pub fn commit_url(&self, sha: &str) -> String {
        match self.host {
            Host::GitLab => format!("{}/-/commit/{sha}", self.web),
            Host::Bitbucket => format!("{}/commits/{sha}", self.web),
            _ => format!("{}/commit/{sha}", self.web),
        }
    }

    pub fn issue_url(&self, number: &str) -> String {
        match self.host {
            Host::GitLab => format!("{}/-/issues/{number}", self.web),
            Host::Bitbucket => format!("{}/pull-requests/{number}", self.web),
            _ => format!("{}/issues/{number}", self.web),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes() {
        let expected = Remote {
            web: "https://github.com/owner/repo".into(),
            host: Host::GitHub,
        };
        for url in [
            "https://github.com/owner/repo.git",
            "https://user:token@github.com/owner/repo",
            "git@github.com:owner/repo.git",
            "ssh://git@github.com:22/owner/repo.git",
            "git://github.com/owner/repo.git",
        ] {
            assert_eq!(Remote::parse(url).as_ref(), Some(&expected), "{url}");
        }
        let gitlab = Remote::parse("git@gitlab.com:group/sub/repo.git").unwrap();
        assert_eq!(gitlab.web, "https://gitlab.com/group/sub/repo");
        assert_eq!(
            gitlab.commit_url("abc"),
            "https://gitlab.com/group/sub/repo/-/commit/abc"
        );
        assert_eq!(
            expected.commit_url("abc"),
            "https://github.com/owner/repo/commit/abc"
        );
        assert!(Remote::parse("not a url").is_none());
        assert!(Remote::parse("file:///Volumes/Backup/repo.git").is_none());
        assert!(Remote::parse("/Volumes/Backup/repo.git").is_none());
        assert!(Remote::parse("gh:acme/payroll.git").is_none());
        // Nothing that could end a Markdown link early or add one.
        assert_eq!(
            Remote::parse("git@github.com:o/r) [x](file:///tmp/y").unwrap().web,
            "https://github.com/o/r%29%20%5Bx%5D%28file%3A///tmp/y"
        );
        assert_eq!(Remote::parse("https://github.com/o/r<x>").unwrap().web, "https://github.com/o/r%3Cx%3E");
        assert!(Remote::parse("https://git hub.com/o/r").is_none());
        assert!(Remote::parse("https://github.com)/o/r").is_none());
        // Names services allow keep their link.
        assert_eq!(
            Remote::parse("https://dev.azure.com/org/Project%20(Legacy)/_git/Repo").unwrap().web,
            "https://dev.azure.com/org/Project%20%28Legacy%29/_git/Repo"
        );
        assert_eq!(Remote::parse("git@git_server.example.com:team/repo.git").unwrap().web, "https://git_server.example.com/team/repo");
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("revs-git-{name}-{}", crate::view::unique_id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn repository_records_must_be_small_regular_files() {
        // A `.git` that is a FIFO would block a plain read; a link to /dev/zero never ends.
        let dir = scratch("special");
        let fifo = std::ffi::CString::new(dir.join(".git").to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(Repo::discover(&dir.join("file.txt")).is_none());
        fs::remove_file(dir.join(".git")).unwrap();
        std::os::unix::fs::symlink("/dev/zero", dir.join(".git")).unwrap();
        assert!(Repo::discover(&dir.join("file.txt")).is_none());
        fs::remove_file(dir.join(".git")).unwrap();

        // HEAD naming a path outside the repository's refs is not followed.
        fs::create_dir_all(dir.join(".git/refs/heads")).unwrap();
        let oid = "1f3c2e9a".repeat(5);
        fs::write(dir.join(".git/refs/heads/main"), format!("{oid}\n")).unwrap();
        fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let repo = Repo::discover(&dir.join("file.txt")).unwrap();
        assert_eq!(repo.read_head().as_deref(), Some(oid.as_str()));
        fs::write(dir.join("outside"), format!("{oid}\n")).unwrap();
        fs::write(dir.join(".git/HEAD"), "ref: refs/../../outside\n").unwrap();
        assert_eq!(repo.read_head(), None);
        fs::write(dir.join(".git/HEAD"), "not an id\n").unwrap();
        assert_eq!(repo.read_head(), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn head_in_other_ref_storage() {
        // reftable keeps HEAD as "ref: refs/heads/.invalid"; git itself then answers.
        let dir = scratch("reftable");
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .unwrap()
        };
        if !git(&["init", "-q", "--ref-format=reftable"]).status.success() {
            return; // a git without reftable
        }
        fs::write(dir.join("f.txt"), "x\n").unwrap();
        git(&["add", "f.txt"]);
        git(&["-c", "commit.gpgsign=false", "commit", "-q", "-m", "c"]);
        let expected = String::from_utf8_lossy(&git(&["rev-parse", "HEAD"]).stdout).trim().to_string();
        let repo = Repo::discover(&dir.join("f.txt")).unwrap();
        assert_eq!(repo.head(), Some(expected));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn working_files_stay_in_the_working_tree() {
        let dir = scratch("working");
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/a.txt"), "inside\n").unwrap();
        let repo = Repo::discover(&dir.join("sub/a.txt")).unwrap();
        assert_eq!(repo.read_working("sub/a.txt").unwrap(), b"inside\n");
        // History can name paths git itself refuses to check out.
        for path in ["sub/../../outside.txt", "../outside.txt", "/etc/hosts", "", "sub"] {
            assert!(repo.read_working(path).is_err(), "{path}");
        }
        std::os::unix::fs::symlink("/dev/zero", dir.join("zero")).unwrap();
        assert!(repo.read_working("zero").is_err());
        // A symlinked folder can't lead out of the working tree; a tracked symlink is the
        // file git checked out.
        fs::write(dir.join("outside.txt"), "outside\n").unwrap();
        std::os::unix::fs::symlink("..", dir.join("sub/up")).unwrap();
        assert!(repo.read_working("sub/up/outside.txt").is_err());
        std::os::unix::fs::symlink("a.txt", dir.join("sub/link.txt")).unwrap();
        assert_eq!(repo.read_working("sub/link.txt").unwrap(), b"inside\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn diff_lines_that_look_like_headers() {
        // A removed line starting with "--" shows up as "---" inside a hunk.
        let diff = "--- a/x\n+++ b/x\n@@ -1 +1 @@\n--- old comment\n+++ new comment\n";
        let hunks = parse_hunks(diff);
        assert_eq!(hunks[0].old_lines, vec!["-- old comment"]);
        assert_eq!(hunks[0].new_lines, vec!["++ new comment"]);
    }
}
