//! Line history: the change before and after a line, as a pair of file versions to diff.
//!
//! PREVIOUS shows the commit that last changed the line (against its parent). On a line
//! the shown commit itself changed, it steps to the line's older version first, so every
//! step reveals an earlier change. NEXT follows the line forward (`git blame --reverse`)
//! to the commit that changed or removed it, ending at the working tree. Every result
//! names the exact line it is about (`Focus`), so the next step continues from there.

use crate::blame::{Blame, Commit};
use crate::diff::{self, NewToOld, OldToNew};
use crate::format;
use crate::git::{self, Blobs, Repo};
use crate::view::{EMPTY, Side, WORKING};
use std::io;
use std::rc::Rc;

/// The line a diff is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    /// 0-based line of the right side.
    Right(u32),
    /// 0-based line of the left side (a removed line).
    Left(u32),
}

/// What a step leads to. (Short-lived, so the size difference between variants is fine.)
#[derive(Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum Outcome {
    /// Diff `left ↔ right`, cursor on 0-based right `line`, about `focus`.
    Open {
        left: Side,
        right: Side,
        line: u32,
        focus: Focus,
        /// Shown with the commit (e.g. "the file was added here").
        note: Option<String>,
    },
    /// Nothing further; the message says why.
    Stay(String),
    /// Readings that lead to different commits; the user picks one.
    Choose {
        question: String,
        choices: Vec<(String, Outcome)>,
    },
}

/// "8a7b6c5 Speed up exports for large repositories…"
pub fn commit_label(sha: &str, summary: &str) -> String {
    let short = format::short_sha(sha);
    if summary.is_empty() {
        short.to_string()
    } else {
        format!("{short} {}", format::truncate(summary, 60))
    }
}

pub fn side_label(repo: &Repo, side: &Side) -> String {
    match side.rev.as_str() {
        WORKING => "the working tree".into(),
        EMPTY => "an empty file".into(),
        sha => {
            let summary = if side.summary.is_empty() {
                repo.describe(sha).map(|(_, _, _, summary)| summary).unwrap_or_default()
            } else {
                side.summary.clone()
            };
            commit_label(sha, &summary)
        }
    }
}

/// The diff that shows `commit`'s change, cursor on the blamed line.
fn show(commit: &Commit, orig: u32) -> Outcome {
    let right = Side {
        rev: commit.sha.clone(),
        path: commit.filename.clone(),
        author: commit.author.clone(),
        mail: commit.mail.clone(),
        time: commit.time,
        summary: commit.summary.clone(),
    };
    let (left, note) = match &commit.previous {
        Some((parent, path)) => (Side::commit(parent, path), None),
        None if commit.boundary => (
            Side::empty(&commit.filename),
            Some("the repository's first commit (or the edge of a shallow clone)".into()),
        ),
        None => (Side::empty(&commit.filename), Some("the file was added here".into())),
    };
    Outcome::Open {
        left,
        right,
        line: orig,
        focus: Focus::Right(orig),
        note,
    }
}

pub fn line_count(text: &[u8]) -> usize {
    if text.is_empty() {
        return 0;
    }
    let newlines = text.iter().filter(|&&byte| byte == b'\n').count();
    newlines + usize::from(text.last() != Some(&b'\n'))
}

/// The lines of `text` (without line endings), for repeated lookups.
pub fn lines_of(text: &[u8]) -> Vec<&[u8]> {
    text.split(|&byte| byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .collect()
}

fn line_at(lines: &[&[u8]], line: u32) -> String {
    lines
        .get(line as usize)
        .map(|content| String::from_utf8_lossy(content).into_owned())
        .unwrap_or_default()
}

fn out_of_range(text: &[u8], line: u32) -> Option<Outcome> {
    (line as usize >= line_count(text)).then(|| Outcome::Stay("There's no line here".into()))
}

/// When a commit was made, for putting a few commits in history order.
struct Stamp {
    sha: String,
    /// Committer time (author time survives rebases and cherry-picks, so it can run
    /// backwards).
    committed: i64,
    authored: i64,
}

impl Stamp {
    fn of(commit: &Commit) -> Stamp {
        Stamp {
            sha: commit.sha.clone(),
            committed: if commit.committed != 0 { commit.committed } else { commit.time },
            authored: commit.time,
        }
    }
}

/// Indices of `stamps`, newest first: by committer time, then — among commits from the
/// same second, as a rebase makes them — by ancestry, then by author time.
fn newest_first(repo: &Repo, stamps: &[Stamp]) -> Vec<usize> {
    let descent: Vec<usize> = stamps
        .iter()
        .map(|stamp| {
            stamps
                .iter()
                .filter(|other| other.sha != stamp.sha && other.committed == stamp.committed)
                .filter(|other| repo.is_ancestor(&other.sha, &stamp.sha))
                .count()
        })
        .collect();
    let mut order: Vec<usize> = (0..stamps.len()).collect();
    order.sort_by_key(|&index| std::cmp::Reverse((stamps[index].committed, descent[index], stamps[index].authored)));
    order
}

/// Lines `first..=last` of `side` blamed. For the working tree `text` is its contents;
/// `pinned` is the commit (and path) the working tree is compared with in a diff, so
/// later commits don't change the answer.
fn blame_lines(
    repo: &Repo,
    side: &Side,
    first: u32,
    last: u32,
    text: &[u8],
    pinned: Option<&Side>,
) -> io::Result<Blame> {
    match side.rev.as_str() {
        WORKING => {
            let base = pinned.filter(|side| side.is_commit());
            repo.blame(
                base.map_or(side.path.as_str(), |base| base.path.as_str()),
                text,
                Some((first, last)),
                base.map(|base| base.rev.as_str()),
                last as usize + 1,
            )
        }
        EMPTY => Err(io::Error::other("nothing to blame in an empty file")),
        sha => repo.blame_rev(sha, &side.path, first, last),
    }
}

fn blame_line(repo: &Repo, side: &Side, line: u32, text: &[u8], pinned: Option<&Side>) -> io::Result<(Commit, u32)> {
    let blame = blame_lines(repo, side, line, line, text, pinned)?;
    let (entry, commit) = blame
        .line(line as usize)
        .ok_or_else(|| io::Error::other("git blame gave no answer for this line"))?;
    let commit = commit
        .cloned()
        .ok_or_else(|| io::Error::other("git blame gave no answer for this line"))?;
    Ok((commit, entry.orig))
}

/// A commit side's contents (empty for the working tree and an absent file).
pub fn text_of(blobs: &Blobs, side: &Side) -> Rc<[u8]> {
    match side.rev.as_str() {
        EMPTY | WORKING => Rc::from(Vec::new()),
        sha => blobs.get(sha, &side.path),
    }
}

fn hunks_between(old: &[u8], new: &[u8]) -> Vec<diff::Hunk> {
    diff::hunks(&String::from_utf8_lossy(old), &String::from_utf8_lossy(new))
}

// ---- PREVIOUS -----------------------------------------------------------------------

/// PREVIOUS for a line of the working file (`text`: its current contents with the line
/// endings it has on disk). `known` is the line's blame if already at hand.
pub fn previous_in_file(
    blobs: &Blobs,
    path: &str,
    text: &[u8],
    line: u32,
    known: Option<(Commit, u32)>,
) -> io::Result<Outcome> {
    if let Some(stay) = out_of_range(text, line) {
        return Ok(stay);
    }
    let (commit, orig) = match known {
        Some(known) => known,
        None => blame_line(blobs.repo(), &Side::working(path), line, text, None)?,
    };
    if !commit.is_uncommitted() {
        return Ok(show(&commit, orig));
    }
    // An uncommitted line: its "commit" is the working tree on top of HEAD (git names
    // HEAD and the file's path there, following staged renames).
    let Some((head, head_path)) = &commit.previous else {
        return Ok(Outcome::Stay("This file isn't committed yet".into()));
    };
    Ok(Outcome::Open {
        left: Side::commit(head, head_path),
        right: Side::working(path),
        line,
        focus: Focus::Right(line),
        note: None,
    })
}

/// The newest commit that changed any of the lines `first..=last`, skipping `skip` (the
/// change already on screen), with the first line it owns.
fn newest_change(repo: &Repo, blame: &Blame, first: u32, last: u32, skip: Option<&str>) -> Option<(Commit, u32)> {
    let mut found: Vec<(Commit, u32)> = Vec::new();
    for line in first..=last {
        let Some((entry, Some(commit))) = blame.line(line as usize) else {
            continue;
        };
        if commit.is_uncommitted()
            || Some(commit.sha.as_str()) == skip
            || found.iter().any(|(known, _)| known.sha == commit.sha)
        {
            continue;
        }
        found.push((commit.clone(), entry.orig));
    }
    let stamps: Vec<Stamp> = found.iter().map(|(commit, _)| Stamp::of(commit)).collect();
    let newest = *newest_first(repo, &stamps).first()?;
    Some(found.swap_remove(newest))
}

/// PREVIOUS for a selection in the working file.
pub fn previous_section_in_file(blobs: &Blobs, path: &str, text: &[u8], first: u32, last: u32) -> io::Result<Outcome> {
    let last = last.min(line_count(text).saturating_sub(1) as u32);
    if first > last {
        return Ok(Outcome::Stay("There's no line here".into()));
    }
    let blame = blame_lines(blobs.repo(), &Side::working(path), first, last, text, None)?;
    if let Some((commit, orig)) = newest_change(blobs.repo(), &blame, first, last, None) {
        return Ok(show(&commit, orig));
    }
    // Every line is uncommitted.
    previous_in_file(blobs, path, text, first, None)
}

/// PREVIOUS for line `line` of the right side of a diff (`right_text`: its contents).
pub fn previous_on_right(blobs: &Blobs, left: &Side, right: &Side, right_text: &[u8], line: u32) -> io::Result<Outcome> {
    let repo = blobs.repo();
    if right.rev == EMPTY {
        return Ok(Outcome::Stay("The file doesn't exist on this side".into()));
    }
    if let Some(stay) = out_of_range(right_text, line) {
        return Ok(stay);
    }
    let (commit, orig) = blame_line(repo, right, line, right_text, Some(left))?;
    let changed_here = commit.is_uncommitted() || commit.sha == right.rev;
    if !changed_here {
        return Ok(show(&commit, orig));
    }
    // This diff already shows the line's change: go to its older version. (For the
    // working tree git names the pinned commit as the parent.)
    let Some((parent, parent_path)) = &commit.previous else {
        return Ok(Outcome::Stay(format!(
            "This line was added with the file in {}; there's no earlier version",
            side_label(repo, right)
        )));
    };
    let parent = Side::commit(parent, parent_path);
    let hunks = hunks_between(&text_of(blobs, &parent), right_text);
    match diff::new_to_old(&hunks, line) {
        NewToOld::Added => Ok(Outcome::Stay(format!(
            "This line was added in {}; there's no earlier version",
            side_label(repo, right)
        ))),
        NewToOld::Changed(older) | NewToOld::Unchanged(older) => {
            let (commit, orig) = blame_line(repo, &parent, older, &[], None)?;
            Ok(show(&commit, orig))
        }
    }
}

/// PREVIOUS for a selection on the right side of a diff.
pub fn previous_section_on_right(
    blobs: &Blobs,
    left: &Side,
    right: &Side,
    right_text: &[u8],
    first: u32,
    last: u32,
) -> io::Result<Outcome> {
    let repo = blobs.repo();
    let last = last.min(line_count(right_text).saturating_sub(1) as u32);
    if right.rev == EMPTY || first > last {
        return Ok(Outcome::Stay("There's no line here".into()));
    }
    let blame = blame_lines(repo, right, first, last, right_text, Some(left))?;
    let skip = right.is_commit().then_some(right.rev.as_str());
    if let Some((commit, orig)) = newest_change(repo, &blame, first, last, skip) {
        return Ok(show(&commit, orig));
    }
    // Every line changed here: look at the older version of the section.
    if left.rev == EMPTY {
        return Ok(Outcome::Stay(format!(
            "These lines were added with the file in {}; there's no earlier version",
            side_label(repo, right)
        )));
    }
    let left_text = text_of(blobs, left);
    let hunks = hunks_between(&left_text, right_text);
    // The older version spans from the first to the last selected line that has one
    // (only the ends are mapped).
    let older = |line: u32| match diff::new_to_old(&hunks, line) {
        NewToOld::Changed(older) | NewToOld::Unchanged(older) => Some(older),
        NewToOld::Added => None,
    };
    let (Some(start), Some(end)) = ((first..=last).find_map(older), (first..=last).rev().find_map(older)) else {
        return Ok(Outcome::Stay(format!(
            "These lines were added in {}; there's no earlier version",
            side_label(repo, right)
        )));
    };
    let (low, high) = (start.min(end), start.max(end));
    let blame = blame_lines(repo, left, low, high, &left_text, None)?;
    match newest_change(repo, &blame, low, high, None) {
        Some((commit, orig)) => Ok(show(&commit, orig)),
        None => Ok(Outcome::Stay("No earlier change found for these lines".into())),
    }
}

/// PREVIOUS for left-side lines `first..=last`: one line, or the removed lines of a
/// block when the exact one can't be known. Blank and brace-only lines don't count when
/// others are there; several commits left → the user picks (newest first).
pub fn previous_on_left(blobs: &Blobs, left: &Side, right: &Side, first: u32, last: u32) -> io::Result<Outcome> {
    let repo = blobs.repo();
    if left.rev == EMPTY {
        return Ok(Outcome::Stay(format!(
            "The file was added in {}; there's no earlier version",
            side_label(repo, right)
        )));
    }
    let blame = repo.blame_rev(&left.rev, &left.path, first, last)?;
    let left_text = text_of(blobs, left);
    let lines = lines_of(&left_text);
    let rows: Vec<u32> = (first..=last).collect();
    let meaningful: Vec<u32> = rows
        .iter()
        .copied()
        .filter(|&line| first == last || !diff::is_trivial(&line_at(&lines, line)))
        .collect();
    let rows = if meaningful.is_empty() { rows } else { meaningful };
    // Distinct commits, each with the first line it owns.
    let mut picks: Vec<(Commit, u32, u32)> = Vec::new();
    for line in rows {
        let Some((entry, Some(commit))) = blame.line(line as usize) else {
            continue;
        };
        if !picks.iter().any(|(known, _, _)| known.sha == commit.sha) {
            picks.push((commit.clone(), entry.orig, line));
        }
    }
    let stamps: Vec<Stamp> = picks.iter().map(|(commit, _, _)| Stamp::of(commit)).collect();
    let picks: Vec<(Commit, u32, u32)> = newest_first(repo, &stamps).into_iter().map(|index| picks[index].clone()).collect();
    match picks.len() {
        0 => Err(io::Error::other("git blame gave no answer for this line")),
        1 => Ok(show(&picks[0].0, picks[0].1)),
        _ => {
            let choices: Vec<(String, Outcome)> = picks
                .iter()
                .map(|(commit, orig, line)| {
                    (
                        format!(
                            "{}: {} — {}",
                            line + 1,
                            format::truncate(line_at(&lines, *line).trim(), 40),
                            format::short_sha(&commit.sha)
                        ),
                        show(commit, *orig),
                    )
                })
                .collect();
            Ok(paged(
                "These removed lines came from different commits. Which line?",
                choices,
            ))
        }
    }
}

/// A question with at most five answers; "Older…" asks again with the rest.
pub fn paged(question: &str, mut choices: Vec<(String, Outcome)>) -> Outcome {
    const PAGE: usize = 5;
    if choices.len() > PAGE {
        let rest = choices.split_off(PAGE - 1);
        choices.push(("Older…".into(), paged(question, rest)));
    }
    Outcome::Choose {
        question: question.into(),
        choices,
    }
}

// ---- NEXT ---------------------------------------------------------------------------

/// NEXT for line `line` of a commit side (`working`: the working file's current text, if
/// it's open with unsaved changes).
pub fn next_from(blobs: &Blobs, from: &Side, line: u32, working: Option<&[u8]>) -> io::Result<Outcome> {
    let repo = blobs.repo();
    match from.rev.as_str() {
        WORKING => {
            return Ok(Outcome::Stay(
                "This is the working file: there's nothing newer. Revs: Open Working File opens it".into(),
            ));
        }
        EMPTY => return Ok(Outcome::Stay("The file doesn't exist on this side".into())),
        _ => {}
    }
    let head = repo.head().ok_or_else(|| io::Error::other("can't read HEAD"))?;
    let (last, last_path, last_line) = if from.rev == head {
        (head.clone(), from.path.clone(), line)
    } else {
        let (commit, orig) = repo.reverse_blame(&from.rev, &head, &from.path, line)?;
        if let Some((next, next_path)) = &commit.previous {
            let next_text = blobs.get(next, next_path);
            let hunks = hunks_between(&blobs.get(&commit.sha, &commit.filename), &next_text);
            let mapped = diff::old_to_new(&hunks, orig);
            let last_row = line_count(&next_text).saturating_sub(1) as u32;
            return Ok(landing(
                Side::commit(&commit.sha, &commit.filename),
                Side::commit(next, next_path),
                mapped,
                orig,
                last_row,
                "the line was removed here",
            ));
        }
        if commit.sha != head {
            if !repo.is_ancestor(&from.rev, &head) {
                return Ok(Outcome::Stay(
                    "This revision isn't in the current branch's history".into(),
                ));
            }
            // The file itself disappears (deleted, or renamed beyond recognition).
            let gone = repo.next_commit_touching(&commit.sha, &head, &commit.filename);
            let mut right = Side::empty(&commit.filename);
            if let Some(gone) = &gone {
                right.summary = format!("deleted in {}", format::short_sha(gone));
            }
            return Ok(Outcome::Open {
                left: Side::commit(&commit.sha, &commit.filename),
                right,
                line: 0,
                focus: Focus::Left(orig),
                note: Some(match gone {
                    Some(gone) => format!(
                        "the file was deleted or renamed in {}",
                        side_label(repo, &Side::commit(&gone, &commit.filename))
                    ),
                    None => "the file was deleted or renamed after this".into(),
                }),
            });
        }
        (head.clone(), commit.filename.clone(), orig)
    };
    // Unchanged up to HEAD: compare with the working tree.
    let disk;
    let working_text = match working {
        Some(text) => text,
        None => match repo.read_working(&last_path) {
            Ok(text) => {
                disk = text;
                &disk
            }
            Err(_) => {
                return Ok(Outcome::Open {
                    left: Side::commit(&last, &last_path),
                    right: Side::empty(&last_path),
                    line: 0,
                    focus: Focus::Left(last_line),
                    note: Some("the file is deleted in the working tree".into()),
                });
            }
        },
    };
    let hunks = hunks_between(&blobs.get(&last, &last_path), working_text);
    let mapped = diff::old_to_new(&hunks, last_line);
    if let OldToNew::Unchanged(_) = mapped {
        return Ok(Outcome::Stay(
            "No later changes: the line is the same in the working file".into(),
        ));
    }
    let last_row = line_count(working_text).saturating_sub(1) as u32;
    Ok(landing(
        Side::commit(&last, &last_path),
        Side::working(&last_path),
        mapped,
        last_line,
        last_row,
        "the line was removed in the working tree",
    ))
}

/// Where NEXT lands: on the line's new version, or — when it was removed — on the line
/// above the removal (so a click on a removed line is a real cursor move) with the
/// removed line itself as the focus.
fn landing(left: Side, right: Side, mapped: OldToNew, old_line: u32, last_row: u32, removed: &str) -> Outcome {
    match mapped {
        OldToNew::Changed(line) | OldToNew::Unchanged(line) => Outcome::Open {
            left,
            right,
            line: line.min(last_row),
            focus: Focus::Right(line.min(last_row)),
            note: None,
        },
        OldToNew::Deleted(after) | OldToNew::Replaced(after) => Outcome::Open {
            left,
            right,
            line: after.saturating_sub(1).min(last_row),
            focus: Focus::Left(old_line),
            note: Some(removed.into()),
        },
    }
}

/// NEXT for a left-side (removed or replaced) line `line` of the diff `left ↔ right`: the
/// shown change removed it, unless it moved — then its next change after `right`.
pub fn next_on_left(
    blobs: &Blobs,
    left: &Side,
    right: &Side,
    right_text: &[u8],
    line: u32,
    working: Option<&[u8]>,
) -> io::Result<Outcome> {
    let hunks = hunks_between(&text_of(blobs, left), right_text);
    match diff::old_to_new(&hunks, line) {
        mapped if mapped.survives() => next_from(blobs, right, mapped.line(), working),
        _ => Ok(Outcome::Stay(format!(
            "This line was removed in {}, the change shown here",
            side_label(blobs.repo(), right)
        ))),
    }
}

/// NEXT for a selection: the earliest later commit that changed any of its lines, or —
/// when none did — the working tree if it changed any of them.
pub fn next_section(blobs: &Blobs, from: &Side, first: u32, last: u32, working: Option<&[u8]>) -> io::Result<Outcome> {
    let repo = blobs.repo();
    let head = repo.head().ok_or_else(|| io::Error::other("can't read HEAD"))?;
    if !from.is_commit() {
        return next_from(blobs, from, first, working);
    }
    // Each selected line's commit that changed it next, or its place in HEAD.
    let mut survivors: Vec<(u32, String)> = Vec::new(); // (line at HEAD, path at HEAD)
    let mut candidates: Vec<(String, u32)> = Vec::new();
    if from.rev == head {
        survivors = (first..=last).map(|line| (line, from.path.clone())).collect();
    } else {
        let blame = repo.reverse_blame_lines(&from.rev, &head, &from.path, first, last)?;
        for line in first..=last {
            let Some((entry, Some(commit))) = blame.line(line as usize) else {
                continue;
            };
            match &commit.previous {
                Some((next, _)) => {
                    if !candidates.iter().any(|(known, _)| known == next) {
                        candidates.push((next.clone(), line));
                    }
                }
                None if commit.sha == head => survivors.push((entry.orig, commit.filename.clone())),
                None => {}
            }
        }
    }
    if !candidates.is_empty() {
        let shas: Vec<String> = candidates.iter().map(|(sha, _)| sha.clone()).collect();
        let times = git::commit_times(repo, &shas);
        let stamps: Vec<Stamp> = candidates
            .iter()
            .map(|(sha, _)| {
                let (committed, authored) = times
                    .iter()
                    .find(|(known, _, _)| known == sha)
                    .map_or((i64::MAX, i64::MAX), |(_, committed, authored)| (*committed, *authored));
                Stamp {
                    sha: sha.clone(),
                    committed,
                    authored,
                }
            })
            .collect();
        let earliest = newest_first(repo, &stamps).last().map_or(first, |&index| candidates[index].1);
        return next_from(blobs, from, earliest, working);
    }
    // No later commit changed them: does the working tree change any of them?
    let Some((_, path)) = survivors.first() else {
        return next_from(blobs, from, first, working);
    };
    let disk;
    let working_text = match working {
        Some(text) => text,
        None => match repo.read_working(path) {
            Ok(text) => {
                disk = text;
                &disk
            }
            Err(_) => return next_from(blobs, from, first, working),
        },
    };
    // HEAD ↔ working once per path (the survivors share one in practice).
    let mut compared: Option<(&str, Vec<diff::Hunk>)> = None;
    for (head_line, path) in &survivors {
        if compared.as_ref().is_none_or(|(known, _)| *known != path.as_str()) {
            compared = Some((path.as_str(), hunks_between(&blobs.get(&head, path), working_text)));
        }
        let Some((_, hunks)) = &compared else {
            continue;
        };
        if !matches!(diff::old_to_new(hunks, *head_line), OldToNew::Unchanged(_)) {
            return next_from(blobs, &Side::commit(&head, path), *head_line, Some(working_text));
        }
    }
    Ok(Outcome::Stay(
        "No later changes: these lines are the same in the working file".into(),
    ))
}

// ---- OPEN FILE ----------------------------------------------------------------------

/// The working file's line for line `line` of `side` (with the side's text), and a
/// note when the line has changed since. Follows the line through history to HEAD
/// (renames included), then into the working text.
pub fn working_line(blobs: &Blobs, side: &Side, side_text: &[u8], line: u32, working: &[u8]) -> (u32, Option<String>) {
    let repo = blobs.repo();
    let approximate = |note: &str| {
        let hunks = hunks_between(side_text, working);
        (diff::map_line(&hunks, line), Some(note.to_string()))
    };
    let (head, head_line, head_path) = match side.rev.as_str() {
        WORKING => {
            let hunks = hunks_between(side_text, working);
            let mapped = diff::old_to_new(&hunks, line);
            return (
                mapped.line(),
                (!mapped.survives()).then(|| "that line is gone now; this is where it was".into()),
            );
        }
        EMPTY => return (0, None),
        sha => {
            let Some(head) = repo.head() else {
                return approximate("couldn't read HEAD; this is the nearest line");
            };
            if sha == head {
                (head, line, side.path.clone())
            } else {
                match repo.reverse_blame(sha, &head, &side.path, line) {
                    Ok((commit, orig)) if commit.sha == head && commit.previous.is_none() => {
                        (head, orig, commit.filename.clone())
                    }
                    Ok(_) => return approximate("that line has changed since; this is the nearest line"),
                    Err(_) => return approximate("couldn't follow that line; this is the nearest line"),
                }
            }
        }
    };
    let head_text = blobs.get(&head, &head_path);
    if head_text.is_empty() && !blobs.exists(&head, &head_path) {
        return approximate("couldn't follow that line; this is the nearest line");
    }
    let hunks = hunks_between(&head_text, working);
    let mapped = diff::old_to_new(&hunks, head_line);
    (
        mapped.line(),
        (!mapped.survives()).then(|| "that line has uncommitted changes; this is where it was".into()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_lines() {
        assert_eq!(line_count(b""), 0);
        assert_eq!(line_count(b"a"), 1);
        assert_eq!(line_count(b"a\n"), 1);
        assert_eq!(line_count(b"a\nb"), 2);
        assert_eq!(line_count(b"a\nb\n"), 2);
    }

    #[test]
    fn labels_and_lines() {
        assert_eq!(commit_label("8a7b6c5d4e3f2a1b", ""), "8a7b6c5");
        assert_eq!(commit_label("8a7b6c5d4e3f2a1b", "Fix"), "8a7b6c5 Fix");
        let lines = lines_of(b"a\r\nb\n");
        assert_eq!(line_at(&lines, 0), "a");
        assert_eq!(line_at(&lines, 1), "b");
        assert_eq!(line_at(&lines, 9), "");
    }

    #[test]
    fn landings() {
        let (left, right) = (Side::commit("a", "f"), Side::commit("b", "f"));
        assert!(matches!(
            landing(left.clone(), right.clone(), OldToNew::Changed(7), 6, 100, "x"),
            Outcome::Open { line: 7, focus: Focus::Right(7), note: None, .. }
        ));
        // Removed: cursor on the line above the removal, focus on the removed line.
        assert!(matches!(
            landing(left.clone(), right.clone(), OldToNew::Deleted(5), 9, 100, "x"),
            Outcome::Open { line: 4, focus: Focus::Left(9), note: Some(_), .. }
        ));
        assert!(matches!(
            landing(left, right, OldToNew::Deleted(0), 0, 100, "x"),
            Outcome::Open { line: 0, focus: Focus::Left(0), .. }
        ));
    }

    #[test]
    fn same_second_commits_follow_history() {
        // A rebase gives replayed commits one committer second and keeps their author times,
        // which here run backwards: history decides.
        let dir = std::env::temp_dir().join(format!("revs-order-{}", crate::view::unique_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str], date: &str| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", "@1700000000 +0000")
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        git(&["init", "-q"], "@0 +0000");
        let mut shas = Vec::new();
        for (index, author) in ["@1600000300 +0000", "@1600000200 +0000", "@1600000100 +0000"].iter().enumerate() {
            std::fs::write(dir.join("f.txt"), format!("{index}\n")).unwrap();
            git(&["add", "f.txt"], author);
            git(&["-c", "commit.gpgsign=false", "commit", "-q", "-m", "c"], author);
            shas.push(git(&["rev-parse", "HEAD"], author));
        }
        let repo = Repo::discover(&dir).unwrap();
        let stamps: Vec<Stamp> = shas
            .iter()
            .zip([300, 200, 100])
            .map(|(sha, authored)| Stamp {
                sha: sha.clone(),
                committed: 1700000000,
                authored: 1600000000 + authored,
            })
            .collect();
        assert_eq!(newest_first(&repo, &stamps), vec![2, 1, 0]);
        // Without ties, committer time alone decides (no git calls).
        let plain = [
            Stamp { sha: "a".repeat(40), committed: 5, authored: 9 },
            Stamp { sha: "b".repeat(40), committed: 7, authored: 1 },
        ];
        assert_eq!(newest_first(&repo, &plain), vec![1, 0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn long_questions_page() {
        let choices: Vec<(String, Outcome)> = (0..12)
            .map(|n| (format!("choice {n}"), Outcome::Stay(String::new())))
            .collect();
        let Outcome::Choose { choices: first, .. } = paged("q", choices) else {
            panic!("not a question");
        };
        assert_eq!(first.len(), 5);
        assert_eq!(first[4].0, "Older…");
        let Outcome::Choose { choices: second, .. } = &first[4].1 else {
            panic!("no second page");
        };
        assert_eq!(second.len(), 5);
        let Outcome::Choose { choices: third, .. } = &second[4].1 else {
            panic!("no third page");
        };
        assert_eq!(third.len(), 4); // 4 + 4 + 4 = 12
    }
}
