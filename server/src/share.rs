//! Unsaved text across Revs processes.
//!
//! Zed runs one language server per project folder, and hands every diff to one of them,
//! so the Revs that steps through a diff may not be the one that has the working file
//! open. Each process lists the working files it has unsaved changes in
//! (`.dirty/<pid>.json`); a process that needs one of them asks for its text by writing
//! `.wants/<id>.want` (the file's path), and the process that has it answers with
//! `.wants/<id>.text`. Processes wait for requests in a kqueue, so nothing runs while idle.

use crate::view::{cache_dir, read_record, unique_id};
use std::fs;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn dirty_dir() -> PathBuf {
    cache_dir().join(".dirty")
}

fn wants_dir() -> PathBuf {
    cache_dir().join(".wants")
}

fn private_dir(dir: &Path) -> bool {
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    true
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Writes a file whole (others never see it half written).
fn write_private(path: &Path, bytes: &[u8]) -> bool {
    let tmp = path.with_extension(format!("tmp-{}", unique_id()));
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| std::io::Write::write_all(&mut file, bytes));
    written.is_ok() && fs::rename(&tmp, path).is_ok()
}

/// Publishes the working files this process has unsaved changes in.
pub fn list_dirty(paths: &[PathBuf]) {
    let path = dirty_dir().join(format!("{}.json", std::process::id()));
    if paths.is_empty() {
        let _ = fs::remove_file(path);
        return;
    }
    if private_dir(&dirty_dir()) {
        let list: Vec<String> = paths.iter().map(|path| path.to_string_lossy().into_owned()).collect();
        write_private(&path, &serde_json::to_vec(&list).unwrap_or_default());
    }
}

pub fn remove_dirty_list() {
    let _ = fs::remove_file(dirty_dir().join(format!("{}.json", std::process::id())));
}

/// Whether another running Revs has unsaved changes in `path`.
fn dirty_elsewhere(path: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dirty_dir()) else {
        return false;
    };
    let own = std::process::id() as i32;
    for entry in entries.filter_map(Result::ok) {
        let file = entry.path();
        let Some(pid) = file.file_stem().and_then(|stem| stem.to_str()).and_then(|stem| stem.parse::<i32>().ok())
        else {
            continue;
        };
        if pid == own {
            continue;
        }
        if !alive(pid) {
            let _ = fs::remove_file(&file);
            continue;
        }
        let listed = read_record(&file, 16 << 20)
            .and_then(|bytes| serde_json::from_slice::<Vec<String>>(&bytes).ok())
            .is_some_and(|list| list.iter().any(|known| Path::new(known) == path));
        if listed {
            return true;
        }
    }
    false
}

/// The unsaved text of `path` from the Revs process that has it open, if any does.
pub fn unsaved_text(path: &Path) -> Option<Vec<u8>> {
    if !dirty_elsewhere(path) || !private_dir(&wants_dir()) {
        return None;
    }
    let id = unique_id();
    let want = wants_dir().join(format!("{id}.want"));
    let answer = wants_dir().join(format!("{id}.text"));
    if !write_private(&want, path.to_string_lossy().as_bytes()) {
        return None;
    }
    let started = Instant::now();
    let text = loop {
        if let Ok(text) = fs::read(&answer) {
            break Some(text);
        }
        if started.elapsed() > Duration::from_millis(500) {
            break None;
        }
        std::thread::sleep(Duration::from_millis(4));
    };
    let _ = fs::remove_file(&want);
    let _ = fs::remove_file(&answer);
    text
}

/// Answers a request with a working file's text, while the process that asked (the id is
/// `<pid>-<n>`) is still waiting.
pub fn answer(id: &str, text: &[u8]) {
    let asker = id.split('-').next().and_then(|pid| pid.parse::<i32>().ok());
    if id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && asker.is_some_and(|pid| pid > 0 && alive(pid))
        && wants_dir().join(format!("{id}.want")).exists()
    {
        write_private(&wants_dir().join(format!("{id}.text")), text);
    }
}

/// Waits for requests (on a thread of its own) and hands each one, as (id, path), to
/// `handle`. Requests older than a minute are leftovers and are removed.
pub fn watch(handle: impl Fn(String, PathBuf) + Send + 'static) {
    if !private_dir(&wants_dir()) {
        return;
    }
    std::thread::spawn(move || {
        let queue = unsafe { libc::kqueue() };
        if queue < 0 {
            return;
        }
        // Watches the folder; again after it is removed and made anew.
        let open = || -> Option<fs::File> {
            private_dir(&wants_dir());
            let dir = fs::File::open(wants_dir()).ok()?;
            let watch = libc::kevent {
                ident: dir.as_raw_fd() as usize,
                filter: libc::EVFILT_VNODE,
                flags: libc::EV_ADD | libc::EV_CLEAR,
                fflags: libc::NOTE_WRITE | libc::NOTE_DELETE | libc::NOTE_RENAME,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            let added = unsafe { libc::kevent(queue, &watch, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
            (added == 0).then_some(dir)
        };
        let Some(mut dir) = open() else {
            return;
        };
        let mut seen: Vec<String> = Vec::new();
        loop {
            let mut event: libc::kevent = unsafe { std::mem::zeroed() };
            let count = unsafe { libc::kevent(queue, std::ptr::null(), 0, &mut event, 1, std::ptr::null()) };
            if count < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            if event.fflags & (libc::NOTE_DELETE | libc::NOTE_RENAME) != 0 {
                // Closing the old folder's descriptor drops its watch.
                drop(dir);
                std::thread::sleep(Duration::from_millis(50));
                let Some(reopened) = open() else {
                    return;
                };
                dir = reopened;
            }
            let Ok(entries) = fs::read_dir(wants_dir()) else {
                continue;
            };
            let mut current = Vec::new();
            for entry in entries.filter_map(Result::ok) {
                let name = entry.file_name().to_string_lossy().into_owned();
                let stale = entry
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age > Duration::from_secs(60)));
                let Some(id) = name.strip_suffix(".want") else {
                    // Answers nobody collected (their asker is gone).
                    if stale {
                        let _ = fs::remove_file(entry.path());
                    }
                    continue;
                };
                current.push(id.to_string());
                if seen.iter().any(|known| known == id) {
                    continue;
                }
                if stale {
                    let _ = fs::remove_file(entry.path());
                    continue;
                }
                if let Some(path) = read_record(&entry.path(), 4096) {
                    handle(id.to_string(), PathBuf::from(String::from_utf8_lossy(&path).as_ref()));
                }
            }
            // Requests are answered once; forget the ones that are gone.
            seen = current;
        }
    });
}
