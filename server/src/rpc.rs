//! JSON-RPC framing for the Language Server Protocol over stdio.

use serde_json::Value;
use std::io::{self, BufRead, Write};

/// Messages larger than this are skipped unread: a file this big is beyond what Revs is for,
/// and a bogus length must not be allocated.
const MAX_MESSAGE: u64 = 1 << 30;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Reads one `Content-Length` framed message. `Ok(None)` means the client closed stdin.
pub fn read_message(reader: &mut impl BufRead) -> io::Result<Option<Value>> {
    loop {
        let mut content_length: Option<u64> = None;
        let mut headers = 0;
        let mut line = String::new();
        loop {
            line.clear();
            if io::Read::take(&mut *reader, 4096).read_line(&mut line)? == 0 {
                return Ok(None);
            }
            if !line.ends_with('\n') {
                return Err(invalid("header line too long"));
            }
            let header = line.trim_end_matches(['\r', '\n']);
            if header.is_empty() {
                if content_length.is_some() {
                    break;
                }
                if headers > 0 {
                    return Err(invalid("missing Content-Length"));
                }
                continue;
            }
            headers += 1;
            if let Some((name, value)) = header.split_once(':')
                && name.trim().eq_ignore_ascii_case("content-length")
            {
                content_length = Some(value.trim().parse().map_err(|_| invalid("bad Content-Length"))?);
            }
        }
        let length = content_length.unwrap_or_default();
        if length > MAX_MESSAGE {
            io::copy(&mut io::Read::take(&mut *reader, length), &mut io::sink())?;
            continue;
        }
        let mut body = vec![0u8; length as usize];
        reader.read_exact(&mut body)?;
        return serde_json::from_slice(&body)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
    }
}

pub fn write_message(out: &mut impl Write, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message)?;
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(&body)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trip() {
        let mut buffer = Vec::new();
        write_message(&mut buffer, &json!({"jsonrpc": "2.0", "id": 1, "result": null})).unwrap();
        write_message(&mut buffer, &json!({"jsonrpc": "2.0", "method": "exit"})).unwrap();
        let mut reader = io::BufReader::new(&buffer[..]);
        assert_eq!(read_message(&mut reader).unwrap().unwrap()["id"], 1);
        assert_eq!(read_message(&mut reader).unwrap().unwrap()["method"], "exit");
        assert!(read_message(&mut reader).unwrap().is_none());
    }

    #[test]
    fn bad_frames() {
        let read = |bytes: &[u8]| read_message(&mut io::BufReader::new(bytes));
        // Lengths are checked before anything is allocated.
        assert!(read(b"Content-Length: 18446744073709551615\r\n\r\n").unwrap().is_none());
        assert!(read(b"Content-Length: 99999999999999999999\r\n\r\n").is_err());
        assert!(read(b"Content-Length: -5\r\n\r\n{}").is_err());
        assert!(read(b"Content-Type: x\r\n\r\n{}").is_err());
        assert!(read(&[b'x'; 10_000]).is_err());
        // An oversized message is skipped and the next one read.
        use std::io::Read as _;
        let header = format!("Content-Length: {}\r\n\r\n", MAX_MESSAGE + 1);
        let mut next = Vec::new();
        write_message(&mut next, &json!({"id": 2})).unwrap();
        let stream = header.as_bytes().chain(io::repeat(b' ').take(MAX_MESSAGE + 1)).chain(&next[..]);
        assert_eq!(read_message(&mut io::BufReader::new(stream)).unwrap().unwrap()["id"], 2);
    }
}
