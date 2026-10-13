//! Text shown to the user: relative/absolute dates, truncation, links, URL encoding.

use crate::git::Remote;

/// "just now", "5 minutes ago", "yesterday", "last month", "2 years ago".
pub fn relative_time(then: i64, now: i64) -> String {
    let seconds = (now - then).max(0);
    if seconds < 45 {
        return "just now".into();
    }
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    let (value, unit, previous) = if seconds < 45 * MINUTE {
        ((seconds + 30) / MINUTE, "minute", "a minute ago")
    } else if seconds < 22 * HOUR {
        ((seconds + HOUR / 2) / HOUR, "hour", "an hour ago")
    } else if seconds < 6 * DAY + DAY / 2 {
        ((seconds + DAY / 2) / DAY, "day", "yesterday")
    } else if seconds < 26 * DAY {
        ((seconds + 3 * DAY) / (7 * DAY), "week", "last week")
    } else if seconds < 320 * DAY {
        ((seconds + 15 * DAY) / (30 * DAY), "month", "last month")
    } else {
        ((seconds + 182 * DAY) / (365 * DAY), "year", "last year")
    };
    if value <= 1 {
        previous.into()
    } else {
        format!("{value} {unit}s ago")
    }
}

/// "August 15th, 2026 12:00 PM" in the local time zone.
pub fn absolute_time(epoch: i64) -> String {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let time = epoch as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return String::new();
    }
    let day = tm.tm_mday;
    let suffix = match (day % 10, day % 100) {
        (1, 11) | (2, 12) | (3, 13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    let hour12 = match tm.tm_hour % 12 {
        0 => 12,
        hour => hour,
    };
    format!(
        "{} {}{}, {} {}:{:02} {}",
        MONTHS[tm.tm_mon.clamp(0, 11) as usize],
        day,
        suffix,
        tm.tm_year + 1900,
        hour12,
        tm.tm_min,
        if tm.tm_hour < 12 { "AM" } else { "PM" }
    )
}

/// "Jan 5, 2026" in the local time zone.
pub fn absolute_date(epoch: i64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let time = epoch as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return String::new();
    }
    format!(
        "{} {}, {}",
        MONTHS[tm.tm_mon.clamp(0, 11) as usize],
        tm.tm_mday,
        tm.tm_year + 1900
    )
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

pub fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}

pub fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// Escapes Markdown control characters in plain text.
pub fn escape_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(
            ch,
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' | '|' | '~'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Escapes text and turns `#123` references into issue/PR links.
pub fn linkify_message(text: &str, remote: Option<&Remote>) -> String {
    let Some(remote) = remote else {
        return escape_markdown(text);
    };
    let mut out = String::with_capacity(text.len() + 32);
    let mut rest = text;
    while let Some(index) = rest.find('#') {
        let (before, after) = rest.split_at(index);
        let digits: String = after[1..]
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect();
        let preceded_by_word = before
            .chars()
            .last()
            .is_some_and(|ch| ch.is_alphanumeric() || ch == '&');
        out.push_str(&escape_markdown(before));
        if digits.is_empty() || preceded_by_word {
            out.push_str("\\#");
            rest = &after[1..];
        } else {
            out.push_str(&format!(
                "[#{digits}]({})",
                remote.issue_url(&digits)
            ));
            rest = &after[1 + digits.len()..];
        }
    }
    out.push_str(&escape_markdown(rest));
    out
}

#[cfg(test)]
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parses `a=1&b=2` into pairs.
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

/// A `file://` URI encoded the way Zed encodes them (the `url` crate's path rules), so
/// URIs Revs builds match the ones Zed sends.
pub fn file_uri(path: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut out = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        let encode = !(0x20..0x7f).contains(&byte)
            || matches!(byte, b' ' | b'"' | b'#' | b'%' | b'<' | b'>' | b'?' | b'`' | b'{' | b'}');
        if encode {
            out.push_str(&format!("%{byte:02X}"));
        } else {
            out.push(byte as char);
        }
    }
    out
}

pub fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    let path = uri.strip_prefix("file://")?;
    // Drop an authority like "localhost".
    let path = if path.starts_with('/') {
        path
    } else {
        &path[path.find('/')?..]
    };
    Some(std::path::PathBuf::from(percent_decode(path)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Remote;

    #[test]
    fn relative_times() {
        let now = 1_000_000_000;
        assert_eq!(relative_time(now - 10, now), "just now");
        assert_eq!(relative_time(now - 60, now), "a minute ago");
        assert_eq!(relative_time(now - 5 * 60, now), "5 minutes ago");
        assert_eq!(relative_time(now - 3600, now), "an hour ago");
        assert_eq!(relative_time(now - 5 * 3600, now), "5 hours ago");
        assert_eq!(relative_time(now - 30 * 3600, now), "yesterday");
        assert_eq!(relative_time(now - 3 * 86400, now), "3 days ago");
        assert_eq!(relative_time(now - 8 * 86400, now), "last week");
        assert_eq!(relative_time(now - 17 * 86400, now), "2 weeks ago");
        assert_eq!(relative_time(now - 40 * 86400, now), "last month");
        assert_eq!(relative_time(now - 100 * 86400, now), "3 months ago");
        assert_eq!(relative_time(now - 400 * 86400, now), "last year");
        assert_eq!(relative_time(now - 800 * 86400, now), "2 years ago");
    }

    #[test]
    fn absolute_format() {
        let text = absolute_time(1786795200);
        assert!(text.contains("2026"), "{text}");
        assert!(text.ends_with("AM") || text.ends_with("PM"), "{text}");
        let date = absolute_date(1786795200);
        assert!(date.starts_with("Aug ") && date.ends_with(", 2026"), "{date}");
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a long commit message", 10), "a long co…");
        assert_eq!(truncate("words and more", 7), "words…");
    }

    #[test]
    fn linkify() {
        let remote = Remote::parse("git@github.com:o/r.git").unwrap();
        assert_eq!(
            linkify_message("Fix *bugs* (#12) and a#3", Some(&remote)),
            "Fix \\*bugs\\* ([#12](https://github.com/o/r/issues/12)) and a\\#3"
        );
        assert_eq!(linkify_message("# not", Some(&remote)), "\\# not");
    }

    #[test]
    fn url_round_trip() {
        let value = "/tmp/My Repo/a&b=c#d.php";
        let query = format!("path={}&line=4", percent_encode(value));
        let parsed = parse_query(&query);
        assert_eq!(parsed[0], ("path".into(), value.into()));
        assert_eq!(parsed[1], ("line".into(), "4".into()));
        let path = std::path::Path::new(value);
        assert_eq!(uri_to_path(&file_uri(path)).unwrap(), path);
        // Like Zed (url crate): parentheses stay, spaces and non-ASCII are encoded.
        assert_eq!(
            file_uri(std::path::Path::new("/a/(auth)/1f3c2e9 ∅.tsx")),
            "file:///a/(auth)/1f3c2e9%20%E2%88%85.tsx"
        );
        assert_eq!(
            uri_to_path("file://localhost/tmp/x").unwrap(),
            std::path::Path::new("/tmp/x")
        );
    }
}
