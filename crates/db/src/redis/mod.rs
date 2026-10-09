//! Redis: a key-value store, so it has no [`Driver`](crate::Driver) or SQL dialect. The app
//! browses keys and edits typed values through [`browse`] and runs single commands through
//! [`run_console`].
//!
//! The client speaks RESP2 itself ([`resp`]) over tokio, with rustls for TLS, so it needs
//! no Redis crate (see `docs/DECISIONS.md`).

pub mod browse;
pub mod client;
pub mod command;
pub mod resp;

pub use browse::{KeyDetails, KeyEdit, KeyEntry, KeyKind, KeyValue, ScanPage, StreamEntry};
pub use client::RedisClient;
pub use command::CommandClass;
pub use resp::Reply;

use crate::error::{DbError, Result};

/// `DbConfig::options` key of the key browser's tree delimiter.
pub const TREE_DELIMITER_OPTION: &str = "tree_delimiter";

/// The tree delimiter when the connection sets none (Redis Insight's default).
pub const DEFAULT_TREE_DELIMITER: &str = ":";

/// The tree delimiter set in a connection's options (`None` or blank: [`DEFAULT_TREE_DELIMITER`]).
pub fn tree_delimiter(option: Option<&str>) -> Vec<u8> {
    match option {
        Some(d) if !d.is_empty() => d.as_bytes().to_vec(),
        _ => DEFAULT_TREE_DELIMITER.as_bytes().to_vec(),
    }
}

/// Run one console command (already split with [`command::split`]). Refuses commands the
/// console can't host, and anything but reads on a read-only connection.
pub async fn run_console(c: &mut RedisClient, args: &[Vec<u8>]) -> Result<Reply> {
    if args.is_empty() {
        return Err(DbError::Param("type a command".into()));
    }
    match command::classify(args) {
        CommandClass::Unsupported => Err(DbError::Unsupported(format!(
            "{} can't run in the console (it blocks or changes the connection); \
             use redis-cli in a terminal",
            command::name(args)
        ))),
        CommandClass::Write | CommandClass::Destructive if c.read_only() => {
            Err(DbError::Unsupported("this connection is read-only".into()))
        }
        _ => c.call(args).await,
    }
}

/// A reply formatted the way `redis-cli` prints it.
pub fn format_reply(r: &Reply) -> String {
    let mut out = String::new();
    write_reply(&mut out, r, 0);
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

fn write_reply(out: &mut String, r: &Reply, indent: usize) {
    match r {
        Reply::Simple(s) => {
            out.push_str(s);
            out.push('\n');
        }
        Reply::Error(e) => {
            out.push_str("(error) ");
            out.push_str(e);
            out.push('\n');
        }
        Reply::Int(i) => {
            out.push_str(&format!("(integer) {i}\n"));
        }
        Reply::Bulk(None) | Reply::Array(None) => out.push_str("(nil)\n"),
        Reply::Bulk(Some(b)) => {
            out.push('"');
            out.push_str(&command::escape(b));
            out.push_str("\"\n");
        }
        Reply::Array(Some(items)) if items.is_empty() => out.push_str("(empty array)\n"),
        Reply::Array(Some(items)) => {
            let width = items.len().to_string().len();
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(&" ".repeat(indent));
                }
                let prefix = format!("{:>width$}) ", i + 1);
                out.push_str(&prefix);
                write_reply(out, item, indent + prefix.len());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_redis_cli() {
        let b = |s: &str| Reply::Bulk(Some(s.as_bytes().to_vec()));
        assert_eq!(format_reply(&Reply::Simple("OK".into())), "OK");
        assert_eq!(format_reply(&Reply::Int(3)), "(integer) 3");
        assert_eq!(format_reply(&Reply::Bulk(None)), "(nil)");
        assert_eq!(format_reply(&b("a\"b\n")), r#""a\"b\n""#);
        assert_eq!(format_reply(&Reply::Array(Some(vec![]))), "(empty array)");
        let nested = Reply::Array(Some(vec![b("a"), Reply::Array(Some(vec![b("x"), b("y")]))]));
        assert_eq!(format_reply(&nested), "1) \"a\"\n2) 1) \"x\"\n   2) \"y\"");
    }
}
