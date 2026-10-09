//! Console commands: `redis-cli`-style argument splitting and what a command may do, so
//! read-only connections, Production confirmation and history redaction can be enforced
//! before anything reaches the server.

use crate::error::{DbError, Result};

/// What a console command does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandClass {
    /// Reads data or server state.
    Read,
    /// Changes data.
    Write,
    /// Removes data in bulk or changes the server (`FLUSHALL`, `DEL`, `CONFIG SET`…):
    /// asks first on Production.
    Destructive,
    /// Changes the connection's protocol state or never returns (`SUBSCRIBE`, `MONITOR`):
    /// the console can't use it.
    Unsupported,
}

/// Split a console line into arguments the way `redis-cli` does: whitespace separates,
/// `"…"` takes `\n \r \t \b \a \\ \" \xHH` escapes, `'…'` takes only `\'`.
pub fn split(line: &str) -> Result<Vec<Vec<u8>>> {
    let mut args = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&first) = chars.peek() else {
            return Ok(args);
        };
        let mut cur = Vec::new();
        match first {
            '"' => {
                chars.next();
                loop {
                    match chars.next() {
                        None => return Err(DbError::Param("unbalanced quotes".into())),
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => cur.push(b'\n'),
                            Some('r') => cur.push(b'\r'),
                            Some('t') => cur.push(b'\t'),
                            Some('b') => cur.push(8),
                            Some('a') => cur.push(7),
                            Some('x') => {
                                let hex: String = chars.by_ref().take(2).collect();
                                let b = u8::from_str_radix(&hex, 16)
                                    .map_err(|_| DbError::Param(format!("bad escape \\x{hex}")))?;
                                cur.push(b);
                            }
                            Some(c) => push_char(&mut cur, c),
                            None => return Err(DbError::Param("unbalanced quotes".into())),
                        },
                        Some(c) => push_char(&mut cur, c),
                    }
                }
            }
            '\'' => {
                chars.next();
                loop {
                    match chars.next() {
                        None => return Err(DbError::Param("unbalanced quotes".into())),
                        Some('\'') => break,
                        Some('\\') if chars.peek() == Some(&'\'') => {
                            chars.next();
                            cur.push(b'\'');
                        }
                        Some(c) => push_char(&mut cur, c),
                    }
                }
            }
            _ => {
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() {
                        break;
                    }
                    chars.next();
                    push_char(&mut cur, c);
                }
            }
        }
        // A closing quote must be followed by a space or the end, as in redis-cli.
        if matches!(first, '"' | '\'') && chars.peek().is_some_and(|c| !c.is_whitespace()) {
            return Err(DbError::Param(
                "closing quote must be followed by a space".into(),
            ));
        }
        args.push(cur);
    }
}

fn push_char(out: &mut Vec<u8>, c: char) {
    let mut b = [0; 4];
    out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
}

/// Upper-cased command name (first argument).
pub fn name(args: &[Vec<u8>]) -> String {
    args.first()
        .map(|a| String::from_utf8_lossy(a).to_ascii_uppercase())
        .unwrap_or_default()
}

fn sub(args: &[Vec<u8>]) -> String {
    args.get(1)
        .map(|a| String::from_utf8_lossy(a).to_ascii_uppercase())
        .unwrap_or_default()
}

/// What `args` would do.
pub fn classify(args: &[Vec<u8>]) -> CommandClass {
    use CommandClass::*;
    let cmd = name(args);
    match cmd.as_str() {
        "SUBSCRIBE" | "PSUBSCRIBE" | "SSUBSCRIBE" | "UNSUBSCRIBE" | "PUNSUBSCRIBE"
        | "SUNSUBSCRIBE" | "MONITOR" | "SYNC" | "PSYNC" | "REPLCONF" | "QUIT" | "RESET"
        | "HELLO" | "SELECT" | "AUTH" | "MULTI" | "EXEC" | "DISCARD" | "WATCH" | "UNWATCH"
        | "SHUTDOWN" | "BLPOP" | "BRPOP" | "BLMOVE" | "BRPOPLPUSH" | "BLMPOP" | "BZPOPMIN"
        | "BZPOPMAX" | "BZMPOP" | "WAIT" | "WAITAOF" => Unsupported,
        "CLIENT" => match sub(args).as_str() {
            "REPLY" | "PAUSE" | "KILL" | "TRACKING" => Unsupported,
            "LIST" | "INFO" | "GETNAME" | "ID" => Read,
            _ => Write,
        },
        // A blocking XREAD/XREADGROUP would hang the console; plain ones read.
        "XREAD" | "XREADGROUP" if has_word(args, "BLOCK") => Unsupported,
        "FLUSHALL" | "FLUSHDB" | "DEL" | "UNLINK" | "SWAPDB" | "MIGRATE" | "MOVE" | "RESTORE"
        | "DEBUG" | "REPLICAOF" | "SLAVEOF" | "FAILOVER" | "BGREWRITEAOF" | "BGSAVE" | "SAVE"
        | "LASTSAVE" | "SCRIPT" | "FUNCTION" | "MODULE" => match (cmd.as_str(), sub(args).as_str())
        {
            ("LASTSAVE", _) => Read,
            ("SCRIPT" | "FUNCTION", "EXISTS" | "LIST" | "DUMP" | "STATS") => Read,
            ("MODULE", "LIST") => Read,
            _ => Destructive,
        },
        "CONFIG" => match sub(args).as_str() {
            "GET" => Read,
            _ => Destructive,
        },
        "ACL" => match sub(args).as_str() {
            "WHOAMI" | "CAT" | "LIST" | "USERS" | "GETUSER" | "LOG" | "DRYRUN" => Read,
            _ => Destructive,
        },
        "CLUSTER" => match sub(args).as_str() {
            "INFO" | "NODES" | "SLOTS" | "SHARDS" | "MYID" | "KEYSLOT" | "COUNTKEYSINSLOT"
            | "GETKEYSINSLOT" | "LINKS" => Read,
            _ => Destructive,
        },
        "EVAL" | "EVALSHA" | "FCALL" => Write,
        "EVAL_RO" | "EVALSHA_RO" | "FCALL_RO" => Read,
        "SORT" if has_word(args, "STORE") => Write,
        _ if READS.contains(&cmd.as_str()) => Read,
        _ => Write,
    }
}

fn has_word(args: &[Vec<u8>], word: &str) -> bool {
    args.iter()
        .skip(1)
        .any(|a| a.eq_ignore_ascii_case(word.as_bytes()))
}

/// Commands that only read (data, keyspace or server state).
const READS: &[&str] = &[
    "GET",
    "MGET",
    "GETRANGE",
    "STRLEN",
    "SUBSTR",
    "LCS",
    "EXISTS",
    "TYPE",
    "TTL",
    "PTTL",
    "EXPIRETIME",
    "PEXPIRETIME",
    "KEYS",
    "SCAN",
    "RANDOMKEY",
    "DBSIZE",
    "DUMP",
    "OBJECT",
    "MEMORY",
    "TOUCH",
    "LLEN",
    "LRANGE",
    "LINDEX",
    "LPOS",
    "SCARD",
    "SMEMBERS",
    "SISMEMBER",
    "SMISMEMBER",
    "SRANDMEMBER",
    "SSCAN",
    "SINTER",
    "SINTERCARD",
    "SUNION",
    "SDIFF",
    "ZCARD",
    "ZCOUNT",
    "ZLEXCOUNT",
    "ZRANGE",
    "ZRANGEBYSCORE",
    "ZRANGEBYLEX",
    "ZREVRANGE",
    "ZREVRANGEBYSCORE",
    "ZREVRANGEBYLEX",
    "ZRANK",
    "ZREVRANK",
    "ZSCORE",
    "ZMSCORE",
    "ZSCAN",
    "ZRANDMEMBER",
    "ZINTER",
    "ZUNION",
    "ZDIFF",
    "ZINTERCARD",
    "HGET",
    "HMGET",
    "HGETALL",
    "HKEYS",
    "HVALS",
    "HLEN",
    "HEXISTS",
    "HSTRLEN",
    "HSCAN",
    "HRANDFIELD",
    "HTTL",
    "HPTTL",
    "XLEN",
    "XRANGE",
    "XREVRANGE",
    "XREAD",
    "XINFO",
    "XPENDING",
    "PFCOUNT",
    "GETBIT",
    "BITCOUNT",
    "BITPOS",
    "BITFIELD_RO",
    "GEOPOS",
    "GEODIST",
    "GEOHASH",
    "GEOSEARCH",
    "GEORADIUS_RO",
    "GEORADIUSBYMEMBER_RO",
    "PING",
    "ECHO",
    "INFO",
    "TIME",
    "ROLE",
    "LOLWUT",
    "COMMAND",
    "SLOWLOG",
    "LATENCY",
    "PUBSUB",
    "SORT_RO",
    "SORT",
    "JSON.GET",
    "JSON.MGET",
    "JSON.TYPE",
    "JSON.STRLEN",
    "JSON.ARRLEN",
    "JSON.OBJKEYS",
    "JSON.OBJLEN",
    "FT.SEARCH",
    "FT.INFO",
    "FT._LIST",
    "FT.AGGREGATE",
    "TS.GET",
    "TS.MGET",
    "TS.RANGE",
    "TS.MRANGE",
    "TS.INFO",
];

/// The line to keep in history: arguments that can carry a password (`AUTH`, `HELLO … AUTH`,
/// `ACL SETUSER`, `CONFIG SET requirepass`, `MIGRATE … AUTH`) are replaced by `***`.
pub fn redacted(args: &[Vec<u8>]) -> String {
    let cmd = name(args);
    let keep = match cmd.as_str() {
        "AUTH" => 1,
        "ACL" if sub(args) == "SETUSER" => 3,
        "CONFIG" if sub(args) == "SET" => 2,
        "HELLO" | "MIGRATE" => args
            .iter()
            .position(|a| a.eq_ignore_ascii_case(b"AUTH") || a.eq_ignore_ascii_case(b"AUTH2"))
            .map_or(args.len(), |i| i + 1),
        _ => args.len(),
    };
    let mut out: Vec<String> = args.iter().take(keep).map(|a| quote(a)).collect();
    if keep < args.len() {
        out.push("***".into());
    }
    out.join(" ")
}

/// An argument as `redis-cli` would accept it back: bare when plain, else double-quoted
/// with escapes.
pub fn quote(a: &[u8]) -> String {
    let plain = !a.is_empty()
        && a.iter()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'"' | b'\'' | b'\\'));
    if plain {
        return String::from_utf8_lossy(a).into_owned();
    }
    format!("\"{}\"", escape(a))
}

/// Bytes as text with non-printable bytes escaped (`\n`, `\xHH`), keeping valid UTF-8.
pub fn escape(a: &[u8]) -> String {
    let mut out = String::with_capacity(a.len());
    for chunk in a.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c.is_control() => {
                    let mut b = [0; 4];
                    for byte in c.encode_utf8(&mut b).bytes() {
                        out.push_str(&format!("\\x{byte:02x}"));
                    }
                }
                c => out.push(c),
            }
        }
        for b in chunk.invalid() {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(line: &str) -> Vec<String> {
        split(line)
            .unwrap()
            .into_iter()
            .map(|a| String::from_utf8(a).unwrap())
            .collect()
    }

    #[test]
    fn splits_like_redis_cli() {
        assert_eq!(s("  SET  k   v "), ["SET", "k", "v"]);
        assert_eq!(s(r#"SET "a b" 'c d'"#), ["SET", "a b", "c d"]);
        assert_eq!(s(r#"SET k "l1\nl2\x41""#), ["SET", "k", "l1\nl2A"]);
        assert_eq!(s(r"SET k 'it\'s'"), ["SET", "k", "it's"]);
        assert_eq!(s(r#"SET k """#), ["SET", "k", ""]);
        assert_eq!(s("GET clé"), ["GET", "clé"]);
        assert!(s("").is_empty());
        assert!(split(r#"SET "open"#).is_err());
        assert!(split(r#"SET "a"b"#).is_err());
        assert!(split(r#"SET "\xZZ""#).is_err());
    }

    #[test]
    fn classifies() {
        let c = |l: &str| classify(&split(l).unwrap());
        assert_eq!(c("get k"), CommandClass::Read);
        assert_eq!(c("HGETALL h"), CommandClass::Read);
        assert_eq!(c("set k v"), CommandClass::Write);
        assert_eq!(c("FLUSHALL"), CommandClass::Destructive);
        assert_eq!(c("del a b"), CommandClass::Destructive);
        assert_eq!(c("CONFIG GET maxmemory"), CommandClass::Read);
        assert_eq!(c("CONFIG SET maxmemory 1"), CommandClass::Destructive);
        assert_eq!(c("SUBSCRIBE ch"), CommandClass::Unsupported);
        assert_eq!(c("select 2"), CommandClass::Unsupported);
        assert_eq!(c("BLPOP q 0"), CommandClass::Unsupported);
        assert_eq!(c("XREAD COUNT 1 STREAMS s 0"), CommandClass::Read);
        assert_eq!(c("XREAD BLOCK 0 STREAMS s $"), CommandClass::Unsupported);
        assert_eq!(c("SORT l"), CommandClass::Read);
        assert_eq!(c("SORT l STORE d"), CommandClass::Write);
        assert_eq!(c("SCRIPT FLUSH"), CommandClass::Destructive);
        assert_eq!(c("SCRIPT EXISTS abc"), CommandClass::Read);
        assert_eq!(c("CLIENT LIST"), CommandClass::Read);
        assert_eq!(c("EVAL 'return 1' 0"), CommandClass::Write);
        assert_eq!(c("SOMEMODULE.CMD x"), CommandClass::Write);
    }

    #[test]
    fn redacts_passwords() {
        let r = |l: &str| redacted(&split(l).unwrap());
        assert_eq!(r("AUTH hunter2"), "AUTH ***");
        assert_eq!(r("AUTH user hunter2"), "AUTH ***");
        assert_eq!(r("HELLO 3 AUTH u p"), "HELLO 3 AUTH ***");
        assert_eq!(r("CONFIG SET requirepass x"), "CONFIG SET ***");
        assert_eq!(r("ACL SETUSER bob on >pw"), "ACL SETUSER bob ***");
        assert_eq!(
            r("MIGRATE h 6379 k 0 5 AUTH pw"),
            "MIGRATE h 6379 k 0 5 AUTH ***"
        );
        assert_eq!(r(r#"SET "a b" v"#), r#"SET "a b" v"#);
    }

    #[test]
    fn quotes_round_trip() {
        for raw in [
            &b"plain"[..],
            b"a b",
            b"",
            b"q\"x",
            b"\x00\xff\n",
            "é".as_bytes(),
        ] {
            let line = format!("SET {}", quote(raw));
            assert_eq!(split(&line).unwrap()[1], raw, "{line}");
        }
    }
}
