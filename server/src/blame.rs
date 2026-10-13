//! `git blame --incremental` parsing and per-line blame kept in sync with buffer edits.

use crate::text::LineEdit;
use std::collections::HashMap;
use std::io::BufRead;

#[derive(Clone, Debug, Default)]
pub struct Commit {
    pub sha: String,
    pub author: String,
    pub mail: String,
    /// Author time (what people see as the commit's date).
    pub time: i64,
    /// Committer time: follows history order, unlike author time after a rebase.
    pub committed: i64,
    pub summary: String,
    /// Parent revision and the file's path there (absent for root/boundary commits).
    /// In `--reverse` mode: the child that changed the line, and the path there.
    pub previous: Option<(String, String)>,
    /// The file's path in this commit.
    pub filename: String,
    /// Root commit, shallow edge, or (with `--reverse`) the start of the range.
    pub boundary: bool,
}

impl Commit {
    /// Lines that only exist in the blamed text (40 zeros, or 64 in SHA-256 repositories).
    pub fn is_uncommitted(&self) -> bool {
        is_zero_sha(&self.sha)
    }
}

pub fn is_zero_sha(sha: &str) -> bool {
    !sha.is_empty() && sha.bytes().all(|byte| byte == b'0')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineBlame {
    /// Index into [`Blame::commits`], or [`EDITED`] for lines changed since the blame ran.
    pub commit: u32,
    /// 0-based line number in the commit's version of the file.
    pub orig: u32,
}

pub const EDITED: u32 = u32::MAX;

#[derive(Clone, Debug, Default)]
pub struct Blame {
    /// 0-based line of the blamed text that `lines[0]` describes.
    pub first: u32,
    pub lines: Vec<LineBlame>,
    pub commits: Vec<Commit>,
}

/// Undoes git's C-style quoting of paths with `"`, `\` or control characters.
pub fn unquote_path(value: &str) -> String {
    let Some(inner) = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return value.to_string();
    };
    let mut out = Vec::with_capacity(inner.len());
    let mut bytes = inner.bytes();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            out.push(byte);
            continue;
        }
        match bytes.next() {
            Some(b'n') => out.push(b'\n'),
            Some(b't') => out.push(b'\t'),
            Some(b'r') => out.push(b'\r'),
            Some(b'a') => out.push(7),
            Some(b'b') => out.push(8),
            Some(b'f') => out.push(12),
            Some(b'v') => out.push(11),
            Some(digit @ b'0'..=b'3') => {
                // Three octal digits encode one byte of a UTF-8 sequence.
                let mut value = (digit - b'0') as u32;
                for _ in 0..2 {
                    match bytes.next() {
                        Some(next @ b'0'..=b'7') => value = value * 8 + (next - b'0') as u32,
                        _ => break,
                    }
                }
                out.push(value as u8);
            }
            Some(other) => out.push(other),
            None => out.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl Blame {
    /// Parses `git blame --incremental` output as it streams in. The result covers
    /// `count` lines starting at 0-based line `first` of the blamed text; lines the output
    /// doesn't mention stay [`EDITED`].
    pub fn parse(mut input: impl BufRead, first: u32, count: usize) -> Self {
        let edited = LineBlame {
            commit: EDITED,
            orig: 0,
        };
        let mut blame = Blame {
            first,
            lines: vec![edited; count],
            commits: Vec::new(),
        };
        let mut index_by_sha: HashMap<String, u32> = HashMap::new();
        // Current entry: commit index, first original line, first final line (1-based), count.
        let mut entry: Option<(u32, u32, u32, u32)> = None;
        let mut raw = Vec::with_capacity(256);

        loop {
            raw.clear();
            match input.read_until(b'\n', &mut raw) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if raw.last() == Some(&b'\n') {
                raw.pop();
            }
            let line = String::from_utf8_lossy(&raw);
            let Some((commit, orig, final_line, lines)) = entry else {
                let fields: Vec<&str> = line.split(' ').collect();
                if fields.len() < 4
                    || fields[0].len() < 40
                    || !fields[0].bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    continue;
                }
                let (Ok(orig), Ok(final_line), Ok(lines)) =
                    (fields[1].parse(), fields[2].parse(), fields[3].parse())
                else {
                    continue;
                };
                let next_index = blame.commits.len() as u32;
                let index = *index_by_sha.entry(fields[0].to_string()).or_insert_with(|| {
                    blame.commits.push(Commit {
                        sha: fields[0].to_string(),
                        ..Commit::default()
                    });
                    next_index
                });
                entry = Some((index, orig, final_line, lines));
                continue;
            };
            let (key, value) = line.split_once(' ').unwrap_or((&line, ""));
            let record = &mut blame.commits[commit as usize];
            match key {
                "author" => record.author = value.to_string(),
                "author-mail" => {
                    record.mail = value
                        .trim_start_matches('<')
                        .trim_end_matches('>')
                        .to_string()
                }
                "author-time" => record.time = value.parse().unwrap_or(0),
                "committer-time" => record.committed = value.parse().unwrap_or(0),
                "summary" => record.summary = value.to_string(),
                "boundary" => record.boundary = true,
                "previous" => {
                    if let Some((sha, path)) = value.split_once(' ') {
                        record.previous = Some((sha.to_string(), unquote_path(path)));
                    }
                }
                // `filename` closes every entry.
                "filename" => {
                    if record.filename.is_empty() {
                        record.filename = unquote_path(value);
                    }
                    let start = final_line.saturating_sub(1);
                    for offset in 0..lines {
                        let target = start + offset;
                        let Some(index) = target.checked_sub(first) else {
                            continue;
                        };
                        if let Some(slot) = blame.lines.get_mut(index as usize) {
                            *slot = LineBlame {
                                commit,
                                orig: orig.saturating_sub(1) + offset,
                            };
                        }
                    }
                    entry = None;
                }
                _ => {}
            }
        }
        blame.lines.shrink_to_fit();
        blame.commits.shrink_to_fit();
        blame
    }

    /// Blame of 0-based line `line` of the blamed text, if it is covered.
    pub fn line(&self, line: usize) -> Option<(LineBlame, Option<&Commit>)> {
        let index = line.checked_sub(self.first as usize)?;
        let entry = *self.lines.get(index)?;
        let commit = self.commits.get(entry.commit as usize);
        Some((entry, commit))
    }

    /// Keeps line attribution aligned with an edit: touched lines become [`EDITED`].
    /// Only used for whole-file blames (`first == 0`).
    pub fn apply_edit(&mut self, edit: LineEdit) {
        let start = edit.start.min(self.lines.len());
        let end = (edit.start + edit.removed + 1).min(self.lines.len());
        let edited = LineBlame {
            commit: EDITED,
            orig: 0,
        };
        self.lines
            .splice(start..end, std::iter::repeat_n(edited, edit.inserted + 1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
1f3c2e9a8b7d6c5e4f3a2b1c0d9e8f7a6b5c4d3e 128 2 2
author Other Person
author-mail <other@example.com>
author-time 1750000000
author-tz +0000
committer Someone
committer-mail <x@example.com>
committer-time 1750000036
committer-tz +0000
summary Group results (#12)
boundary
filename src/new.rs
8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f0a9b 17 1 1
author Sam Example
author-mail <dev@example.com>
author-time 1700000000
author-tz +0000
committer Someone
committer-mail <x@example.com>
committer-time 1700000030
committer-tz +0000
summary Search: support batches (#34)
previous 2b4d6f8a0c1e3a5c7e9b1d3f5a7c9e1b3d5f7a9c src/old.rs
filename src/new.rs
8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f0a9b 100 4 1
filename src/new.rs
";

    #[test]
    fn parses_incremental() {
        let blame = Blame::parse(SAMPLE.as_bytes(), 0, 5);
        assert_eq!(blame.commits.len(), 2);
        let (line, commit) = blame.line(0).unwrap();
        assert_eq!(line.orig, 16);
        let commit = commit.unwrap();
        assert_eq!(commit.author, "Sam Example");
        assert_eq!(commit.mail, "dev@example.com");
        assert_eq!(commit.time, 1700000000);
        assert_eq!(commit.committed, 1700000030);
        assert!(!commit.boundary);
        assert_eq!(
            commit.previous,
            Some((
                "2b4d6f8a0c1e3a5c7e9b1d3f5a7c9e1b3d5f7a9c".into(),
                "src/old.rs".into()
            ))
        );
        assert_eq!(commit.filename, "src/new.rs");
        let (line, commit) = blame.line(1).unwrap();
        assert_eq!(line.orig, 127);
        assert_eq!(commit.unwrap().summary, "Group results (#12)");
        assert!(commit.unwrap().previous.is_none());
        assert!(commit.unwrap().boundary);
        assert_eq!(blame.line(2).unwrap().0.orig, 128);
        assert_eq!(blame.line(3).unwrap().0.commit, 1);
        assert_eq!(blame.line(3).unwrap().0.orig, 99);
        assert_eq!(blame.line(4).unwrap().0.commit, EDITED);
        assert!(blame.line(5).is_none());
    }

    #[test]
    fn parses_a_range() {
        // `-L 2,3`: only final lines 2 and 3 are reported.
        let blame = Blame::parse(SAMPLE.as_bytes(), 1, 2);
        assert!(blame.line(0).is_none());
        assert_eq!(blame.line(1).unwrap().0.orig, 127);
        assert_eq!(blame.line(2).unwrap().0.orig, 128);
        assert!(blame.line(3).is_none());
    }

    #[test]
    fn edits_mark_lines() {
        let mut blame = Blame::parse(SAMPLE.as_bytes(), 0, 4);
        blame.apply_edit(LineEdit {
            start: 1,
            removed: 1,
            inserted: 0,
        });
        assert_eq!(blame.lines.len(), 3);
        assert_eq!(blame.lines[0].commit, 1);
        assert_eq!(blame.lines[1].commit, EDITED);
        assert_eq!(blame.lines[2].commit, 1);
        blame.apply_edit(LineEdit {
            start: 0,
            removed: 0,
            inserted: 2,
        });
        assert_eq!(blame.lines.len(), 5);
        assert_eq!(blame.lines[3].commit, EDITED);
        assert_eq!(blame.lines[4].commit, 1);
    }

    #[test]
    fn zero_shas_and_quoted_paths() {
        assert!(is_zero_sha("0000000000000000000000000000000000000000"));
        assert!(is_zero_sha(&"0".repeat(64)));
        assert!(!is_zero_sha("0000000000000000000000000000000000000001"));
        assert_eq!(unquote_path("plain/path.txt"), "plain/path.txt");
        assert_eq!(unquote_path("\"we\\\"ird.txt\""), "we\"ird.txt");
        assert_eq!(unquote_path("\"tab\\tname.txt\""), "tab\tname.txt");
        assert_eq!(unquote_path("\"back\\\\slash.txt\""), "back\\slash.txt");
        // core.quotepath=on style octal escapes for non-ASCII bytes.
        assert_eq!(unquote_path("\"caf\\303\\251.txt\""), "café.txt");
    }
}
