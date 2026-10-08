//! RESP2, the Redis wire protocol: requests are arrays of bulk strings, replies are one of
//! five types. Only the client side (encode requests, decode replies) is implemented.

use crate::error::{DbError, Result};

/// Largest bulk string accepted (Redis's own `proto-max-bulk-len` default is 512 MB).
const MAX_BULK: usize = 512 * 1024 * 1024;
/// Largest array accepted, and nesting depth, so a hostile server can't exhaust memory.
const MAX_ARRAY: usize = 16 * 1024 * 1024;
const MAX_DEPTH: usize = 64;

/// A reply from the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// `+OK`.
    Simple(String),
    /// `-ERR …`: a command error (the connection stays usable).
    Error(String),
    /// `:42`.
    Int(i64),
    /// `$5 hello`, or `$-1` (nil).
    Bulk(Option<Vec<u8>>),
    /// `*2 …`, or `*-1` (nil).
    Array(Option<Vec<Reply>>),
}

impl Reply {
    /// The bytes of a bulk or simple string.
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Reply::Bulk(Some(b)) => Some(b),
            Reply::Simple(s) => Some(s.as_bytes()),
            _ => None,
        }
    }

    /// The reply as an integer (an `Int`, or a string holding one).
    pub fn int(&self) -> Option<i64> {
        match self {
            Reply::Int(i) => Some(*i),
            other => std::str::from_utf8(other.bytes()?).ok()?.parse().ok(),
        }
    }

    /// The elements of an array reply (empty for nil).
    pub fn items(&self) -> &[Reply] {
        match self {
            Reply::Array(Some(v)) => v,
            _ => &[],
        }
    }

    /// The reply, or the server's error as a [`DbError`].
    pub fn ok(self) -> Result<Reply> {
        match self {
            Reply::Error(e) => Err(server_error(&e)),
            other => Ok(other),
        }
    }
}

/// A server error reply as a [`DbError::Server`], with its prefix (`ERR`, `WRONGTYPE`) as the
/// code.
pub fn server_error(e: &str) -> DbError {
    let (code, message) = match e.split_once(' ') {
        Some((c, m)) if !c.is_empty() && c.chars().all(|c| c.is_ascii_uppercase()) => {
            (Some(c.to_owned()), m.to_owned())
        }
        _ => (None, e.to_owned()),
    };
    DbError::Server(Box::new(crate::error::ServerError {
        severity: "ERROR".into(),
        code,
        message,
        detail: None,
        hint: None,
        position: None,
    }))
}

/// A request: `*<n>\r\n` then each argument as a bulk string.
pub fn encode<A: AsRef<[u8]>>(args: &[A]) -> Vec<u8> {
    let mut out =
        Vec::with_capacity(16 + args.iter().map(|a| a.as_ref().len() + 16).sum::<usize>());
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for a in args {
        let a = a.as_ref();
        out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Decode one reply from the front of `buf`: `Ok(None)` when more bytes are needed,
/// otherwise the reply and how many bytes it used.
pub fn decode(buf: &[u8]) -> Result<Option<(Reply, usize)>> {
    decode_at(buf, 0, 0)
}

fn decode_at(buf: &[u8], pos: usize, depth: usize) -> Result<Option<(Reply, usize)>> {
    if depth > MAX_DEPTH {
        return Err(DbError::Protocol("reply nested too deeply".into()));
    }
    let Some(line_end) = find_crlf(buf, pos + 1) else {
        return Ok(None);
    };
    let Some(&kind) = buf.get(pos) else {
        return Ok(None);
    };
    let line = &buf[pos + 1..line_end];
    let next = line_end + 2;
    let text = || String::from_utf8_lossy(line).into_owned();
    match kind {
        b'+' => Ok(Some((Reply::Simple(text()), next))),
        b'-' => Ok(Some((Reply::Error(text()), next))),
        b':' => Ok(Some((Reply::Int(parse_int(line)?), next))),
        b'$' => {
            let len = parse_int(line)?;
            if len < 0 {
                return Ok(Some((Reply::Bulk(None), next)));
            }
            let len = usize::try_from(len).map_err(|_| bad("bulk length"))?;
            if len > MAX_BULK {
                return Err(DbError::Protocol("bulk reply too large".into()));
            }
            let end = next + len;
            if buf.len() < end + 2 {
                return Ok(None);
            }
            if &buf[end..end + 2] != b"\r\n" {
                return Err(bad("bulk terminator"));
            }
            Ok(Some((Reply::Bulk(Some(buf[next..end].to_vec())), end + 2)))
        }
        b'*' => {
            let n = parse_int(line)?;
            if n < 0 {
                return Ok(Some((Reply::Array(None), next)));
            }
            let n = usize::try_from(n).map_err(|_| bad("array length"))?;
            if n > MAX_ARRAY {
                return Err(DbError::Protocol("array reply too large".into()));
            }
            // Each element takes at least 3 bytes; don't reserve more than the buffer holds.
            let mut items = Vec::with_capacity(n.min(buf.len() / 3 + 1));
            let mut at = next;
            for _ in 0..n {
                match decode_at(buf, at, depth + 1)? {
                    Some((r, used)) => {
                        items.push(r);
                        at = used;
                    }
                    None => return Ok(None),
                }
            }
            Ok(Some((Reply::Array(Some(items)), at)))
        }
        other => Err(DbError::Protocol(format!(
            "unexpected reply type byte 0x{other:02x}"
        ))),
    }
}

fn find_crlf(buf: &[u8], from: usize) -> Option<usize> {
    buf.get(from..)?
        .windows(2)
        .position(|w| w == b"\r\n")
        .map(|i| from + i)
}

fn parse_int(line: &[u8]) -> Result<i64> {
    std::str::from_utf8(line)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad("integer"))
}

fn bad(what: &str) -> DbError {
    DbError::Protocol(format!("malformed {what} in reply"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(b: &[u8]) -> Reply {
        let (r, used) = decode(b).unwrap().unwrap();
        assert_eq!(used, b.len());
        r
    }

    #[test]
    fn encodes_bulk_array() {
        assert_eq!(
            encode(&["SET", "k", "v a"]),
            b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$3\r\nv a\r\n"
        );
    }

    #[test]
    fn decodes_every_type() {
        assert_eq!(one(b"+OK\r\n"), Reply::Simple("OK".into()));
        assert_eq!(one(b"-ERR nope\r\n"), Reply::Error("ERR nope".into()));
        assert_eq!(one(b":-7\r\n"), Reply::Int(-7));
        assert_eq!(one(b"$-1\r\n"), Reply::Bulk(None));
        assert_eq!(one(b"$0\r\n\r\n"), Reply::Bulk(Some(vec![])));
        assert_eq!(
            one(b"$4\r\na\r\nb\r\n"),
            Reply::Bulk(Some(b"a\r\nb".to_vec()))
        );
        assert_eq!(one(b"*-1\r\n"), Reply::Array(None));
        assert_eq!(
            one(b"*2\r\n$1\r\na\r\n*1\r\n:1\r\n"),
            Reply::Array(Some(vec![
                Reply::Bulk(Some(b"a".to_vec())),
                Reply::Array(Some(vec![Reply::Int(1)])),
            ]))
        );
    }

    #[test]
    fn partial_input_needs_more() {
        let full = b"*2\r\n$5\r\nhello\r\n:3\r\n";
        for cut in 0..full.len() {
            assert_eq!(decode(&full[..cut]).unwrap(), None, "cut at {cut}");
        }
        assert!(decode(full).unwrap().is_some());
    }

    #[test]
    fn leaves_following_replies() {
        let (r, used) = decode(b":1\r\n:2\r\n").unwrap().unwrap();
        assert_eq!((r, used), (Reply::Int(1), 4));
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(b"?x\r\n").is_err());
        assert!(decode(b":abc\r\n").is_err());
        assert!(decode(b"$2\r\nabXY").is_err());
    }

    #[test]
    fn server_error_keeps_code() {
        let e = server_error("WRONGTYPE Operation against a key holding the wrong kind of value");
        let s = e.as_server().unwrap();
        assert_eq!(s.code.as_deref(), Some("WRONGTYPE"));
        assert!(s.message.starts_with("Operation"));
        let e = server_error("plain message");
        assert_eq!(e.as_server().unwrap().code, None);
    }
}
