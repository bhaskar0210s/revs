//! Line diffs computed the way Zed draws them, and mapping lines through them.
//!
//! Zed's diff view uses imara-diff's histogram algorithm with git's slider/indent
//! post-processing on line-ending-normalised text (buffer_diff.rs `compute_hunks`). Using
//! the same engine here means a row Revs reasons about is the row Zed shows: which
//! lines sit side by side, where removed lines are, where the cursor lands.

use imara_diff::{Algorithm, Diff, InternedInput, sources::lines};

/// A changed region: `old_start/old_count` lines replaced by `new_start/new_count`
/// (1-based starts as in git hunk headers; a count of 0 means the start is the line
/// *before* the insertion/deletion point), with the removed and added lines.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Hunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
}

impl Hunk {
    /// 0-based first old line (for an insertion: the old line after it).
    pub fn old_first(&self) -> u32 {
        if self.old_count == 0 { self.old_start } else { self.old_start - 1 }
    }

    /// 0-based first new line (for a removal: the new line after it).
    pub fn new_first(&self) -> u32 {
        if self.new_count == 0 { self.new_start } else { self.new_start - 1 }
    }
}

/// `\r\n` and lone `\r` become `\n`, as in Zed's buffers.
pub fn normalize(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('\r') {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(ch);
        }
    }
    std::borrow::Cow::Owned(out)
}

/// The hunks Zed shows between `old` and `new`.
pub fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    let (old, new) = (normalize(old), normalize(new));
    let (old, new) = (old.as_ref(), new.as_ref());
    // Zed shows an emptied buffer ("\n") as one removal of everything.
    if new == "\n" && old.ends_with('\n') && old.len() > 1 {
        let old_lines: Vec<String> = old.lines().map(str::to_string).collect();
        return vec![Hunk {
            old_start: 1,
            old_count: old_lines.len() as u32,
            new_start: 0,
            new_count: 0,
            old_lines,
            new_lines: Vec::new(),
        }];
    }
    let input = InternedInput::new(lines(old), lines(new));
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    let token_text = |token: imara_diff::Token| {
        input.interner[token]
            .trim_end_matches('\n')
            .trim_end_matches('\r')
            .to_string()
    };
    diff.hunks()
        .map(|hunk| {
            let (old_count, new_count) = (hunk.before.len() as u32, hunk.after.len() as u32);
            Hunk {
                old_start: if old_count > 0 { hunk.before.start + 1 } else { hunk.before.start },
                old_count,
                new_start: if new_count > 0 { hunk.after.start + 1 } else { hunk.after.start },
                new_count,
                old_lines: input.before[hunk.before.start as usize..hunk.before.end as usize]
                    .iter()
                    .map(|&token| token_text(token))
                    .collect(),
                new_lines: input.after[hunk.after.start as usize..hunk.after.end as usize]
                    .iter()
                    .map(|&token| token_text(token))
                    .collect(),
            }
        })
        .collect()
}

/// Where a line of the old side ends up on the new side (0-based lines).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OldToNew {
    Unchanged(u32),
    /// Inside a changed block; the line's new version.
    Changed(u32),
    /// Inside a changed block that no longer contains anything like it: where it would
    /// sit in the new block (possibly the line after it).
    Replaced(u32),
    /// Removed; the line that follows the removal.
    Deleted(u32),
}

impl OldToNew {
    pub fn line(self) -> u32 {
        match self {
            OldToNew::Unchanged(line)
            | OldToNew::Changed(line)
            | OldToNew::Replaced(line)
            | OldToNew::Deleted(line) => line,
        }
    }

    pub fn survives(self) -> bool {
        matches!(self, OldToNew::Unchanged(_) | OldToNew::Changed(_))
    }
}

/// Where a line of the new side came from on the old side (0-based lines).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewToOld {
    Unchanged(u32),
    /// Inside a changed block; the line's old version.
    Changed(u32),
    /// A new line with no older version.
    Added,
}

/// How far (in lines) a moved or reflowed line is looked for in other changed blocks.
const MOVE_WINDOW: i64 = 60;

/// Maps a 0-based old line through hunks. Inside a block of the same size lines pair up
/// by position; when the sizes differ the most similar line nearby is used, so a block
/// that grew from 1 to 400 lines doesn't map them all onto one line. A line that left
/// its block (moved a few lines, or reflowed) is found in a nearby block.
pub fn old_to_new(hunks: &[Hunk], line: u32) -> OldToNew {
    let direct = old_to_new_in_block(hunks, line);
    match direct {
        OldToNew::Deleted(_) | OldToNew::Replaced(_) => {
            let Some((index, offset)) = locate_old(hunks, line) else {
                return direct;
            };
            moved_old_to_new(hunks, index, offset).map_or(direct, OldToNew::Changed)
        }
        other => other,
    }
}

/// Maps a 0-based new line back through hunks (see [`old_to_new`]).
pub fn new_to_old(hunks: &[Hunk], line: u32) -> NewToOld {
    let direct = new_to_old_in_block(hunks, line);
    if direct != NewToOld::Added {
        return direct;
    }
    let Some((index, offset)) = locate_new(hunks, line) else {
        return direct;
    };
    moved_new_to_old(hunks, index, offset).map_or(direct, NewToOld::Changed)
}

/// Maps a 0-based old line to the new side (the line itself, or where it went).
pub fn map_line(hunks: &[Hunk], line: u32) -> u32 {
    old_to_new(hunks, line).line()
}

/// The hunk containing old line `line`, and the line's index inside it.
fn locate_old(hunks: &[Hunk], line: u32) -> Option<(usize, usize)> {
    hunks.iter().enumerate().find_map(|(index, hunk)| {
        let first = hunk.old_first();
        (hunk.old_count > 0 && line >= first && line < first + hunk.old_count)
            .then(|| (index, (line - first) as usize))
            .filter(|&(index, offset)| offset < hunks[index].old_lines.len())
    })
}

/// The hunk containing new line `line`, and the line's index inside it.
fn locate_new(hunks: &[Hunk], line: u32) -> Option<(usize, usize)> {
    hunks.iter().enumerate().find_map(|(index, hunk)| {
        let first = hunk.new_first();
        (hunk.new_count > 0 && line >= first && line < first + hunk.new_count)
            .then(|| (index, (line - first) as usize))
            .filter(|&(index, offset)| offset < hunks[index].new_lines.len())
    })
}

/// Where in the other side of a block line `index` (of `count`) is expected.
fn proportional(index: usize, count: usize, other: usize) -> usize {
    if count == 0 || other == 0 {
        return 0;
    }
    (index * other / count).min(other - 1)
}

/// The line of `others` that `lines[index]` became (or came from) inside one changed
/// block of a different size: similar enough, and each the other's best match — unless
/// one is a reflowed piece of the other (then several pieces share one line).
fn partner(lines: &[String], index: usize, others: &[String]) -> Option<usize> {
    let preferred = proportional(index, lines.len(), others.len());
    let (found, piece) = best_match(&lines[index], others, preferred)?;
    if piece {
        return Some(found);
    }
    let back = proportional(found, others.len(), lines.len());
    let (mutual, _) = best_match(&others[found], lines, back)?;
    (mutual == index).then_some(found)
}

/// Whether old line `offset` of `hunk` has a partner inside its own block.
fn old_paired(hunk: &Hunk, offset: usize) -> bool {
    if hunk.new_count == 0 {
        return false;
    }
    hunk.old_count == hunk.new_count || partner(&hunk.old_lines, offset, &hunk.new_lines).is_some()
}

/// Whether new line `offset` of `hunk` has a partner inside its own block.
fn new_paired(hunk: &Hunk, offset: usize) -> bool {
    if hunk.old_count == 0 {
        return false;
    }
    hunk.old_count == hunk.new_count || partner(&hunk.new_lines, offset, &hunk.old_lines).is_some()
}

/// A line that left its block: the best unmatched line of a nearby block on the other
/// side, if the two are nearly the same or one is a reflowed piece of the other, and
/// each is the other's best match.
fn moved_new_to_old(hunks: &[Hunk], index: usize, offset: usize) -> Option<u32> {
    let text = &hunks[index].new_lines[offset];
    let me = hunks[index].new_first() + offset as u32;
    // Where the line would be on the old side had it stayed put; of equal matches (the
    // same edit made in several places) the nearest wins.
    let near = hunks[index].old_first() as i64 + offset as i64;
    let candidates = unmatched(hunks, hunks[index].old_first() as i64, false);
    let (old_line, old_text, piece) = best_moved(text, &candidates, near)?;
    if piece {
        return Some(old_line);
    }
    // Mutual: from that old line, the best unmatched new line nearby (expected the same
    // distance away) must be this one.
    let back = unmatched(hunks, hunks[index].new_first() as i64, true);
    let (new_line, _, _) = best_moved(old_text, &back, me as i64 + (old_line as i64 - near))?;
    (new_line == me).then_some(old_line)
}

fn moved_old_to_new(hunks: &[Hunk], index: usize, offset: usize) -> Option<u32> {
    let text = &hunks[index].old_lines[offset];
    let me = hunks[index].old_first() + offset as u32;
    let near = hunks[index].new_first() as i64 + offset as i64;
    let candidates = unmatched(hunks, hunks[index].new_first() as i64, true);
    let (new_line, new_text, piece) = best_moved(text, &candidates, near)?;
    if piece {
        return Some(new_line);
    }
    let back = unmatched(hunks, hunks[index].old_first() as i64, false);
    let (old_line, _, _) = best_moved(new_text, &back, me as i64 + (new_line as i64 - near))?;
    (old_line == me).then_some(new_line)
}

/// Unmatched lines (removed with no partner, or added with no partner) of the blocks
/// within [`MOVE_WINDOW`] of `near`, on the new side (`new`) or the old side.
fn unmatched(hunks: &[Hunk], near: i64, new: bool) -> Vec<(u32, &str)> {
    let mut lines = Vec::new();
    for hunk in hunks {
        let (first, texts) = if new {
            (hunk.new_first(), &hunk.new_lines)
        } else {
            (hunk.old_first(), &hunk.old_lines)
        };
        if (first as i64 - near).abs() > MOVE_WINDOW {
            continue;
        }
        for (offset, text) in texts.iter().enumerate().take(512) {
            let paired = if new { new_paired(hunk, offset) } else { old_paired(hunk, offset) };
            if !paired {
                lines.push((first + offset as u32, text.as_str()));
            }
        }
    }
    lines
}

/// Across blocks only near-identical lines, or a reflowed piece of a line, count; equal
/// scores go to the line nearest `near`. Returns the line, its text and whether it is a
/// piece match.
fn best_moved<'a>(text: &str, candidates: &[(u32, &'a str)], near: i64) -> Option<(u32, &'a str, bool)> {
    let target = Prepared::new(text);
    if target.chars.iter().filter(|ch| ch.is_alphanumeric()).count() < 3 {
        return None; // blank lines and braces carry no identity
    }
    let mut best: Option<(f64, i64, u32, &str, bool)> = None;
    for &(line, candidate) in candidates {
        let (score, piece) = target.moved_score(candidate);
        let distance = (line as i64 - near).abs();
        if score > 0.0
            && best.is_none_or(|(known, known_distance, ..)| {
                score > known || (score == known && distance < known_distance)
            })
        {
            best = Some((score, distance, line, candidate, piece));
        }
    }
    best.map(|(_, _, line, text, piece)| (line, text, piece))
}

fn old_to_new_in_block(hunks: &[Hunk], line: u32) -> OldToNew {
    let line = line as i64 + 1;
    let mut delta: i64 = 0;
    for hunk in hunks {
        let (old_start, old_count) = (hunk.old_start as i64, hunk.old_count as i64);
        let (new_start, new_count) = (hunk.new_start as i64, hunk.new_count as i64);
        if old_count == 0 {
            // Insertion after old line `old_start`.
            if line <= old_start {
                break;
            }
            delta += new_count;
            continue;
        }
        if line < old_start {
            break;
        }
        if line < old_start + old_count {
            if new_count == 0 {
                return OldToNew::Deleted(new_start.max(0) as u32);
            }
            let index = (line - old_start) as usize;
            if old_count != new_count && hunk.old_lines.len() == old_count as usize {
                return match partner(&hunk.old_lines, index, &hunk.new_lines) {
                    Some(found) => OldToNew::Changed((new_start - 1 + found as i64) as u32),
                    None => OldToNew::Replaced((new_start - 1 + (index as i64).min(new_count)).max(0) as u32),
                };
            }
            return OldToNew::Changed((new_start - 1 + (index as i64).min(new_count - 1)).max(0) as u32);
        }
        delta += new_count - old_count;
    }
    OldToNew::Unchanged((line + delta - 1).max(0) as u32)
}

fn new_to_old_in_block(hunks: &[Hunk], line: u32) -> NewToOld {
    let line = line as i64 + 1;
    let mut delta: i64 = 0;
    for hunk in hunks {
        let (old_start, old_count) = (hunk.old_start as i64, hunk.old_count as i64);
        let (new_start, new_count) = (hunk.new_start as i64, hunk.new_count as i64);
        if new_count == 0 {
            // Deletion: the new side continues after new line `new_start`.
            if line <= new_start {
                break;
            }
            delta += old_count;
            continue;
        }
        if line < new_start {
            break;
        }
        if line < new_start + new_count {
            if old_count == 0 {
                return NewToOld::Added;
            }
            let index = (line - new_start) as usize;
            if old_count != new_count && hunk.new_lines.len() == new_count as usize {
                return match partner(&hunk.new_lines, index, &hunk.old_lines) {
                    Some(found) => NewToOld::Changed((old_start - 1 + found as i64) as u32),
                    None => NewToOld::Added,
                };
            }
            return NewToOld::Changed((old_start - 1 + (index as i64).min(old_count - 1)).max(0) as u32);
        }
        delta += old_count - new_count;
    }
    NewToOld::Unchanged((line + delta - 1).max(0) as u32)
}

pub const SIMILAR: f64 = 0.5;

/// Lines checked around the expected position before giving up on a block.
const MATCH_WINDOW: usize = 64;

/// The candidate most similar to `text`: an identical line anywhere in the block (the one
/// nearest `preferred`), else the most similar line around `preferred` (small blocks are
/// searched whole). Also says whether the match is exact or one line is a reflowed piece
/// of the other.
fn best_match(text: &str, candidates: &[String], preferred: usize) -> Option<(usize, bool)> {
    if candidates.is_empty() {
        return None;
    }
    let target = Prepared::new(text);
    if target.chars.is_empty() {
        return None; // blank lines carry no identity
    }
    let preferred = preferred.min(candidates.len() - 1);
    // A plain comparison, cheap even for blocks of thousands of lines.
    if let Some(index) = (0..candidates.len())
        .filter(|&index| candidates[index].trim() == target.trimmed)
        .min_by_key(|&index| index.abs_diff(preferred))
    {
        return Some((index, true));
    }
    // Similar lines rank by similarity, a little less the further from the expected spot.
    let rank = |index: usize| {
        let (score, piece) = target.score(&candidates[index]);
        (score >= SIMILAR).then(|| (score - 0.001 * index.abs_diff(preferred) as f64, index, piece))
    };
    let better = |a: Option<(f64, usize, bool)>, b: Option<(f64, usize, bool)>| match (a, b) {
        (Some(a), Some(b)) => Some(if b.0 > a.0 { b } else { a }),
        (a, b) => a.or(b),
    };
    // Similarity is costly: score the lines around the expected position.
    let window = preferred.saturating_sub(MATCH_WINDOW)..(preferred + MATCH_WINDOW + 1).min(candidates.len());
    let mut best = window.clone().map(rank).fold(None, better);
    if best.is_none() && candidates.len() <= 512 {
        best = (0..candidates.len())
            .filter(|index| !window.contains(index))
            .map(rank)
            .fold(None, better);
    }
    best.map(|(_, index, piece)| (index, piece))
}

/// A line ready to be compared with many others.
struct Prepared {
    trimmed: String,
    chars: Vec<char>,
    counts: std::collections::HashMap<char, u16>,
}

impl Prepared {
    fn new(text: &str) -> Prepared {
        let trimmed = text.trim().to_string();
        let chars: Vec<char> = trimmed.chars().take(400).collect();
        let mut counts = std::collections::HashMap::new();
        for &ch in &chars {
            *counts.entry(ch).or_insert(0u16) += 1;
        }
        Prepared {
            trimmed,
            chars,
            counts,
        }
    }

    /// Ratcliff/Obershelp similarity (difflib's ratio) ignoring surrounding whitespace.
    fn similarity(&self, other: &str) -> f64 {
        self.score(other).0
    }

    /// The similarity, and whether one line is a piece of the other (a line split out of
    /// a longer one: ≥ 12 chars, 90 % intact). A piece counts as similar.
    fn score(&self, other: &str) -> (f64, bool) {
        let other: Vec<char> = other.trim().chars().take(400).collect();
        if self.chars.is_empty() || other.is_empty() {
            return (0.0, false);
        }
        let total = (self.chars.len() + other.len()) as f64;
        let shorter = self.chars.len().min(other.len());
        let common = self.common(&other);
        let could_match = 2.0 * common as f64 / total >= SIMILAR;
        let could_contain = shorter >= 12 && common as f64 >= 0.9 * shorter as f64;
        if !could_match && !could_contain {
            return (0.0, false);
        }
        let mut score = 0.0;
        if could_match {
            score = 2.0 * matching_chars(&self.chars, &other) as f64 / total;
        }
        if could_contain && self.chars.len() != other.len() {
            let (_, _, size) = longest_common_run(&self.chars, &other);
            if size as f64 >= 0.9 * shorter as f64 {
                return (score.max(SIMILAR), true);
            }
        }
        (score, false)
    }

    /// The stricter rule for lines in different blocks: nearly identical (ratio ≥ 0.9),
    /// or one a reflowed piece of the other (`true`). 0 when neither holds.
    fn moved_score(&self, other: &str) -> (f64, bool) {
        let other: Vec<char> = other.trim().chars().take(400).collect();
        if self.chars.is_empty() || other.is_empty() {
            return (0.0, false);
        }
        let total = (self.chars.len() + other.len()) as f64;
        let shorter = self.chars.len().min(other.len());
        let common = self.common(&other);
        if 2.0 * common as f64 / total >= 0.9 {
            let ratio = 2.0 * matching_chars(&self.chars, &other) as f64 / total;
            if ratio >= 0.9 {
                return (ratio, false);
            }
        }
        if shorter >= 12 && self.chars.len() != other.len() && common as f64 >= 0.9 * shorter as f64 {
            let (_, _, size) = longest_common_run(&self.chars, &other);
            if size as f64 >= 0.9 * shorter as f64 {
                return (0.8, true);
            }
        }
        (0.0, false)
    }

    fn common(&self, other: &[char]) -> usize {
        let mut counts = self.counts.clone();
        let mut common = 0;
        for ch in other {
            if let Some(count) = counts.get_mut(ch)
                && *count > 0
            {
                *count -= 1;
                common += 1;
            }
        }
        common
    }
}

/// Similarity of two lines (see [`Prepared::similarity`]).
pub fn similarity(a: &str, b: &str) -> f64 {
    Prepared::new(a).similarity(b)
}

/// Total size of the matching blocks found by recursively taking the longest common run.
fn matching_chars(a: &[char], b: &[char]) -> usize {
    let mut total = 0;
    let mut pending = vec![(0usize, a.len(), 0usize, b.len())];
    while let Some((a_lo, a_hi, b_lo, b_hi)) = pending.pop() {
        if a_lo >= a_hi || b_lo >= b_hi {
            continue;
        }
        let (i, j, size) = longest_common_run(&a[a_lo..a_hi], &b[b_lo..b_hi]);
        if size == 0 {
            continue;
        }
        total += size;
        pending.push((a_lo, a_lo + i, b_lo, b_lo + j));
        pending.push((a_lo + i + size, a_hi, b_lo + j + size, b_hi));
    }
    total
}

/// Start in `a`, start in `b` and length of the longest common contiguous run.
fn longest_common_run(a: &[char], b: &[char]) -> (usize, usize, usize) {
    let mut best = (0, 0, 0);
    let mut previous = vec![0usize; b.len() + 1];
    let mut current = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        for (j, &cb) in b.iter().enumerate() {
            current[j + 1] = if ca == cb { previous[j] + 1 } else { 0 };
            if current[j + 1] > best.2 {
                best = (i + 1 - current[j + 1], j + 1 - current[j + 1], current[j + 1]);
            }
        }
        std::mem::swap(&mut previous, &mut current);
    }
    best
}

/// Lines that say nothing about history on their own: blank, or only braces and
/// punctuation (`}`, `);`, `*/`).
pub fn is_trivial(line: &str) -> bool {
    !line.chars().any(|ch| ch.is_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(old_start: u32, old_count: u32, new_start: u32, new_count: u32) -> Hunk {
        Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
            ..Hunk::default()
        }
    }

    fn numbers(hunks: &[Hunk]) -> Vec<(u32, u32, u32, u32)> {
        hunks
            .iter()
            .map(|h| (h.old_start, h.old_count, h.new_start, h.new_count))
            .collect()
    }

    #[test]
    fn hunks_follow_git_conventions() {
        // Delete lines 3-4, insert 3 lines after line 10, change line 20.
        let old: String = (1..=25).map(|n| format!("line {n}\n")).collect();
        let mut new_lines: Vec<String> = (1..=25).map(|n| format!("line {n}")).collect();
        new_lines[19] = "changed 20".into();
        new_lines.splice(10..10, ["A".to_string(), "B".into(), "C".into()]);
        new_lines.drain(2..4);
        let new = new_lines.join("\n") + "\n";
        let hunks = hunks(&old, &new);
        assert_eq!(numbers(&hunks), vec![(3, 2, 2, 0), (10, 0, 9, 3), (20, 1, 21, 1)]);
        assert_eq!(hunks[0].old_lines, vec!["line 3", "line 4"]);
        assert_eq!(hunks[1].new_lines, vec!["A", "B", "C"]);
        assert_eq!(map_line(&hunks, 0), 0);
        assert_eq!(old_to_new(&hunks, 2), OldToNew::Deleted(2));
        assert_eq!(map_line(&hunks, 4), 2);
        assert_eq!(map_line(&hunks, 9), 7);
        assert_eq!(map_line(&hunks, 10), 11);
        assert_eq!(old_to_new(&hunks, 19), OldToNew::Changed(20));
        assert_eq!(new_to_old(&hunks, 2), NewToOld::Unchanged(4));
        assert_eq!(new_to_old(&hunks, 8), NewToOld::Added);
        assert_eq!(new_to_old(&hunks, 20), NewToOld::Changed(19));
    }

    #[test]
    fn line_endings_and_emptied_files() {
        let hunks_crlf = hunks("a\r\nb\r\nc\r\n", "a\nB\nc\n");
        assert_eq!(numbers(&hunks_crlf), vec![(2, 1, 2, 1)]);
        assert_eq!(hunks_crlf[0].old_lines, vec!["b"]);
        let emptied = hunks("a\nb\n", "\n");
        assert_eq!(numbers(&emptied), vec![(1, 2, 0, 0)]);
    }

    #[test]
    fn unequal_blocks_map_by_similarity() {
        let mut block = hunk(10, 1, 10, 4);
        block.old_lines = vec!["$post = $this->posts->query()->with('comments')->firstOrFail($id);".into()];
        block.new_lines = vec![
            "Log::info('updating');".into(),
            "$post = $this->posts->query()".into(),
            "    ->with('comments')".into(),
            "    ->firstOrFail($id);".into(),
        ];
        let hunks = vec![block];
        assert_eq!(old_to_new(&hunks, 9), OldToNew::Changed(10));
        assert_eq!(new_to_old(&hunks, 9), NewToOld::Added);
        assert_eq!(new_to_old(&hunks, 10), NewToOld::Changed(9));
        let mut same = hunk(5, 2, 5, 2);
        same.old_lines = vec!["a".into(), "b".into()];
        same.new_lines = vec!["x".into(), "y".into()];
        assert_eq!(old_to_new(&[same.clone()], 5), OldToNew::Changed(5));
        assert_eq!(new_to_old(&[same], 4), NewToOld::Changed(4));
    }

    #[test]
    fn moved_code_is_followed_across_blocks() {
        // A one-line statement removed above `try {` and added back, split over three lines,
        // inside it.
        let old = "{\n$post = $this->posts->query()->with('comments', 'author.profile')->firstOrFail($id);\n\nDB::beginTransaction();\n\ntry {\n    $post->fill($attributes);\n}\n";
        let new = "{\nDB::beginTransaction();\n\ntry {\n    $post = $this->posts->query()\n        ->with('comments', 'author.profile')\n        ->firstOrFail($id);\n\n    $post->fill($attributes);\n}\n";
        let hunks = hunks(old, new);
        // Each part of the reflowed statement maps back to the removed line (old line 1).
        assert_eq!(new_to_old(&hunks, 4), NewToOld::Changed(1));
        assert_eq!(new_to_old(&hunks, 5), NewToOld::Changed(1));
        assert_eq!(new_to_old(&hunks, 6), NewToOld::Changed(1));
        // The blank line added with it has no identity.
        assert_eq!(new_to_old(&hunks, 7), NewToOld::Added);
        // And the removed line is followed forward to its new home.
        assert!(matches!(old_to_new(&hunks, 1), OldToNew::Changed(4..=6)));
    }

    #[test]
    fn the_same_move_in_several_places() {
        // One statement moved into `try {` in three sibling functions: each copy maps to its
        // own, not to the first.
        let function = |name: &str| {
            format!("fn {name}() {{\n    $user = $this->users->find($id);\n    begin();\n    try {{\n        work();\n    }}\n}}\n\n")
        };
        let moved = |name: &str| {
            format!("fn {name}() {{\n    begin();\n    try {{\n        $user = $this->users->find($id);\n        work();\n    }}\n}}\n\n")
        };
        let old: String = ["a", "b", "c"].iter().map(|name| function(name)).collect();
        let new: String = ["a", "b", "c"].iter().map(|name| moved(name)).collect();
        let hunks = hunks(&old, &new);
        for copy in 0..3u32 {
            let (old_line, new_line) = (copy * 8 + 1, copy * 8 + 3);
            assert_eq!(new_to_old(&hunks, new_line), NewToOld::Changed(old_line), "copy {copy}");
            assert_eq!(old_to_new(&hunks, old_line), OldToNew::Changed(new_line), "copy {copy}");
        }
    }

    #[test]
    fn replaced_lines_keep_their_place() {
        // "three" has no counterpart in its block: it maps to where it stood, the line
        // after the block, not the block's first line.
        let hunks = hunks("one\ntwo\nthree\nfour\nfive\n", "ONE\nTWO\nfour\nfive\n");
        assert_eq!(old_to_new(&hunks, 2), OldToNew::Replaced(2));
        assert_eq!(map_line(&hunks, 2), 2);
    }

    #[test]
    fn identical_partners_far_away() {
        // A 90-line branch removed and the else body dedented: the dedented lines are
        // their old selves, far from where the block's proportions would look.
        let mut old = String::from("if ($legacy) {\n");
        for n in 0..90 {
            old.push_str(&format!("    $legacyUser{n} = $this->legacyUsers->find($ids[{n}]);\n"));
        }
        old.push_str("} else {\n    $user = $this->users->find($id);\n    $user->touch();\n    return $user;\n}\nend();\n");
        let new = "$user = $this->users->find($id);\n$user->touch();\nreturn $user;\nend();\n";
        let hunks = hunks(&old, new);
        assert_eq!(new_to_old(&hunks, 0), NewToOld::Changed(92));
        assert_eq!(new_to_old(&hunks, 2), NewToOld::Changed(94));
        assert_eq!(old_to_new(&hunks, 92), OldToNew::Changed(0));
    }

    #[test]
    fn moved_lines_need_a_real_match() {
        // A different statement added below a removed one is not the removed line moved.
        let old = "fn all() {\n    $items = $this->model->with('comments')->get();\n}\n\nfn find() {\n    return null;\n}\n";
        let new = "fn all() {\n}\n\nfn find() {\n    $item = $this->model->with('comments')->find($id);\n    return null;\n}\n";
        let first = hunks(old, new);
        assert_eq!(new_to_old(&first, 4), NewToOld::Added);
        assert!(matches!(old_to_new(&first, 1), OldToNew::Deleted(_)));
        // A line whose own block pairs it (`* @return Post` → `* @return $this`) isn't
        // free to be someone else's moved line.
        let old = "/**\n * @return Post\n */\nfoo();\n";
        let new = "/**\n * @var Comment\n * @return $this\n */\nfoo();\n";
        let second = hunks(old, new);
        assert_eq!(new_to_old(&second, 1), NewToOld::Added);
    }

    #[test]
    fn large_blocks_map_near_their_position() {
        // 6000 rows that all gained a column, plus one inserted row: row 5000 stays 5000.
        let old: String = (0..6000).map(|n| format!("{n},item {n},{}\n", n * 3)).collect();
        let mut rows: Vec<String> = (0..6000).map(|n| format!("{n},item {n},{},flag", n * 3)).collect();
        rows.insert(10, "new,row,0,flag".into());
        let new = rows.join("\n") + "\n";
        let block = hunks(&old, &new);
        let started = std::time::Instant::now();
        assert_eq!(new_to_old(&block, 5001), NewToOld::Changed(5000));
        assert_eq!(old_to_new(&block, 4000), OldToNew::Changed(4001));
        assert!(started.elapsed() < std::time::Duration::from_millis(200), "{:?}", started.elapsed());
    }

    #[test]
    fn similarity_scores() {
        assert!(similarity("    return $this->find($id);", "return $this->find($id);") >= SIMILAR);
        assert_eq!(similarity("", "x"), 0.0);
        assert!(similarity("foo(bar, baz)", "completely different text") < SIMILAR);
        assert!(similarity("->with('comments', 'author.profile')", "$post = $this->posts->with('comments', 'author.profile')->find($id);") >= SIMILAR);
        assert!(is_trivial("") && is_trivial("    });") && is_trivial(" */") && !is_trivial("$x = 1;"));
    }
}
