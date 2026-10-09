//! Keyspace browsing and typed value reads and edits on top of [`RedisClient`].

use serde::{Deserialize, Serialize};

use super::client::RedisClient;
use super::resp::Reply;
use crate::error::{DbError, Result};

/// The type of a key, as `TYPE` reports it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KeyKind {
    /// `string`.
    String,
    /// `list`.
    List,
    /// `set`.
    Set,
    /// `zset` (sorted set).
    ZSet,
    /// `hash`.
    Hash,
    /// `stream`.
    Stream,
    /// RedisJSON's `ReJSON-RL`.
    Json,
    /// The key no longer exists (`none`).
    Missing,
    /// A module type (`TSDB-TYPE`, `MBbloom--`…).
    Other(String),
}

impl KeyKind {
    fn parse(t: &str) -> Self {
        match t {
            "string" => KeyKind::String,
            "list" => KeyKind::List,
            "set" => KeyKind::Set,
            "zset" => KeyKind::ZSet,
            "hash" => KeyKind::Hash,
            "stream" => KeyKind::Stream,
            "ReJSON-RL" => KeyKind::Json,
            "none" => KeyKind::Missing,
            other => KeyKind::Other(other.to_owned()),
        }
    }

    /// Short label for badges (`STR`, `HASH`).
    pub fn badge(&self) -> &str {
        match self {
            KeyKind::String => "STR",
            KeyKind::List => "LIST",
            KeyKind::Set => "SET",
            KeyKind::ZSet => "ZSET",
            KeyKind::Hash => "HASH",
            KeyKind::Stream => "STRM",
            KeyKind::Json => "JSON",
            KeyKind::Missing => "—",
            KeyKind::Other(_) => "MOD",
        }
    }

    /// Readable name (`Sorted set`).
    pub fn label(&self) -> &str {
        match self {
            KeyKind::String => "String",
            KeyKind::List => "List",
            KeyKind::Set => "Set",
            KeyKind::ZSet => "Sorted set",
            KeyKind::Hash => "Hash",
            KeyKind::Stream => "Stream",
            KeyKind::Json => "JSON",
            KeyKind::Missing => "Deleted",
            KeyKind::Other(t) => t,
        }
    }

    /// The kinds a new key can be created as, in menu order.
    pub const CREATABLE: [KeyKind; 6] = [
        KeyKind::String,
        KeyKind::Hash,
        KeyKind::List,
        KeyKind::Set,
        KeyKind::ZSet,
        KeyKind::Stream,
    ];
}

/// A key found by [`scan`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEntry {
    /// Key bytes (keys are binary-safe).
    pub key: Vec<u8>,
    /// Type.
    pub kind: KeyKind,
}

/// One page of `SCAN`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanPage {
    /// Cursor for the next page; 0 when the scan is complete.
    pub cursor: u64,
    /// Keys in this page (may be empty while the cursor is not 0).
    pub keys: Vec<KeyEntry>,
}

/// A stream entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamEntry {
    /// Entry id (`1700000000000-0`).
    pub id: String,
    /// Field / value pairs.
    pub fields: Vec<(Vec<u8>, Vec<u8>)>,
}

/// The value of a key, up to the read limit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum KeyValue {
    /// String bytes (the first `limit` bytes).
    String(Vec<u8>),
    /// List elements from the head.
    List(Vec<Vec<u8>>),
    /// Set members.
    Set(Vec<Vec<u8>>),
    /// Sorted set members and scores, lowest score first.
    ZSet(Vec<(Vec<u8>, f64)>),
    /// Hash fields and values.
    Hash(Vec<(Vec<u8>, Vec<u8>)>),
    /// Stream entries, newest first.
    Stream(Vec<StreamEntry>),
    /// RedisJSON document text.
    Json(String),
    /// A type the browser can't show; use the console.
    Unsupported,
    /// The key no longer exists.
    Missing,
}

/// A key's details for the value pane.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeyDetails {
    /// Key bytes.
    pub key: Vec<u8>,
    /// Type.
    pub kind: KeyKind,
    /// Time to live in ms; `None` when the key has no expiry.
    pub ttl_ms: Option<i64>,
    /// Length: bytes for strings, elements otherwise.
    pub len: u64,
    /// Memory used, when `MEMORY USAGE` is allowed.
    pub memory: Option<u64>,
    /// The value (possibly truncated; compare its size with `len`).
    pub value: KeyValue,
}

impl KeyDetails {
    /// Fewer elements (or bytes) were read than the key holds.
    pub fn truncated(&self) -> bool {
        let shown = match &self.value {
            KeyValue::String(b) => b.len(),
            KeyValue::List(v) | KeyValue::Set(v) => v.len(),
            KeyValue::ZSet(v) => v.len(),
            KeyValue::Hash(v) => v.len(),
            KeyValue::Stream(v) => v.len(),
            _ => return false,
        };
        (shown as u64) < self.len
    }
}

/// A change to one key from the browser.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum KeyEdit {
    /// `SET` the whole string (keeps the TTL).
    SetString(Vec<u8>),
    /// `HSET field value`.
    HashSet(Vec<u8>, Vec<u8>),
    /// `HDEL field`.
    HashDelete(Vec<u8>),
    /// `LSET index value`.
    ListSet(i64, Vec<u8>),
    /// `RPUSH` (tail) or `LPUSH` (head) a value.
    ListPush {
        /// The value.
        value: Vec<u8>,
        /// Push at the head.
        head: bool,
    },
    /// Remove the first element equal to the value (`LREM 1`).
    ListRemove(Vec<u8>),
    /// `SADD member`.
    SetAdd(Vec<u8>),
    /// `SREM member`.
    SetRemove(Vec<u8>),
    /// `ZADD score member`.
    ZAdd(Vec<u8>, f64),
    /// `ZREM member`.
    ZRemove(Vec<u8>),
    /// `XADD * field value…`.
    StreamAdd(Vec<(Vec<u8>, Vec<u8>)>),
    /// `XDEL id`.
    StreamDelete(String),
    /// `EXPIRE seconds`, or `PERSIST` with `None`.
    Expire(Option<u64>),
    /// `RENAMENX` (never overwrites another key).
    Rename(Vec<u8>),
    /// `UNLINK` the key.
    Delete,
}

impl KeyEdit {
    /// A short past-tense description for toasts.
    pub fn done(&self) -> &'static str {
        match self {
            KeyEdit::SetString(_) => "Value saved",
            KeyEdit::HashSet(..) => "Field saved",
            KeyEdit::HashDelete(_) => "Field removed",
            KeyEdit::ListSet(..) => "Element saved",
            KeyEdit::ListPush { .. } => "Element added",
            KeyEdit::ListRemove(_) => "Element removed",
            KeyEdit::SetAdd(_) | KeyEdit::ZAdd(..) => "Member saved",
            KeyEdit::SetRemove(_) | KeyEdit::ZRemove(_) => "Member removed",
            KeyEdit::StreamAdd(_) => "Entry added",
            KeyEdit::StreamDelete(_) => "Entry removed",
            KeyEdit::Expire(Some(_)) => "Expiry set",
            KeyEdit::Expire(None) => "Expiry removed",
            KeyEdit::Rename(_) => "Key renamed",
            KeyEdit::Delete => "Key deleted",
        }
    }

    fn args(&self, key: &[u8]) -> Vec<Vec<u8>> {
        let k = key.to_vec();
        let b = |s: &str| s.as_bytes().to_vec();
        match self {
            KeyEdit::SetString(v) => vec![b("SET"), k, v.clone(), b("KEEPTTL")],
            KeyEdit::HashSet(f, v) => vec![b("HSET"), k, f.clone(), v.clone()],
            KeyEdit::HashDelete(f) => vec![b("HDEL"), k, f.clone()],
            KeyEdit::ListSet(i, v) => vec![b("LSET"), k, b(&i.to_string()), v.clone()],
            KeyEdit::ListPush { value, head } => {
                vec![b(if *head { "LPUSH" } else { "RPUSH" }), k, value.clone()]
            }
            KeyEdit::ListRemove(v) => vec![b("LREM"), k, b("1"), v.clone()],
            KeyEdit::SetAdd(m) => vec![b("SADD"), k, m.clone()],
            KeyEdit::SetRemove(m) => vec![b("SREM"), k, m.clone()],
            KeyEdit::ZAdd(m, s) => vec![b("ZADD"), k, b(&s.to_string()), m.clone()],
            KeyEdit::ZRemove(m) => vec![b("ZREM"), k, m.clone()],
            KeyEdit::StreamAdd(fields) => {
                let mut v = vec![b("XADD"), k, b("*")];
                for (f, val) in fields {
                    v.push(f.clone());
                    v.push(val.clone());
                }
                v
            }
            KeyEdit::StreamDelete(id) => vec![b("XDEL"), k, b(id)],
            KeyEdit::Expire(Some(s)) => vec![b("EXPIRE"), k, b(&s.to_string())],
            KeyEdit::Expire(None) => vec![b("PERSIST"), k],
            KeyEdit::Rename(to) => vec![b("RENAMENX"), k, to.clone()],
            KeyEdit::Delete => vec![b("UNLINK"), k],
        }
    }
}

/// One `SCAN` page matching `pattern` (glob, `*` for all), with each key's type.
pub async fn scan(c: &mut RedisClient, cursor: u64, pattern: &str, count: u32) -> Result<ScanPage> {
    let pattern = if pattern.trim().is_empty() {
        "*"
    } else {
        pattern.trim()
    };
    let r = c
        .call(&[
            "SCAN",
            &cursor.to_string(),
            "MATCH",
            pattern,
            "COUNT",
            &count.max(1).to_string(),
        ])
        .await?
        .ok()?;
    let [next, keys] = r.items() else {
        return Err(DbError::Protocol("unexpected SCAN reply".into()));
    };
    let next = next
        .int()
        .and_then(|n| u64::try_from(n).ok())
        .or_else(|| std::str::from_utf8(next.bytes()?).ok()?.parse().ok())
        .ok_or_else(|| DbError::Protocol("bad SCAN cursor".into()))?;
    let keys: Vec<Vec<u8>> = keys
        .items()
        .iter()
        .filter_map(|k| k.bytes().map(<[u8]>::to_vec))
        .collect();
    let types = if keys.is_empty() {
        Vec::new()
    } else {
        let cmds: Vec<[&[u8]; 2]> = keys.iter().map(|k| [b"TYPE".as_slice(), k]).collect();
        let refs: Vec<&[&[u8]]> = cmds.iter().map(|c| c.as_slice()).collect();
        c.pipeline(&refs).await?
    };
    let mut out: Vec<KeyEntry> = keys
        .into_iter()
        .zip(types)
        .map(|(key, t)| KeyEntry {
            key,
            kind: KeyKind::parse(&String::from_utf8_lossy(t.bytes().unwrap_or(b"none"))),
        })
        .filter(|e| e.kind != KeyKind::Missing)
        .collect();
    out.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(ScanPage {
        cursor: next,
        keys: out,
    })
}

/// Number of keys in the selected database.
pub async fn dbsize(c: &mut RedisClient) -> Result<u64> {
    let r = c.call(&["DBSIZE"]).await?.ok()?;
    r.int()
        .and_then(|n| u64::try_from(n).ok())
        .ok_or_else(|| DbError::Protocol("bad DBSIZE reply".into()))
}

/// A key's type, TTL, size and value; collections read at most `limit` elements and
/// strings at most `limit * 64` bytes.
pub async fn load(c: &mut RedisClient, key: &[u8], limit: usize) -> Result<KeyDetails> {
    let limit = limit.max(1);
    let meta = c
        .pipeline(&[
            &[b"TYPE".as_slice(), key][..],
            &[b"PTTL".as_slice(), key],
            &[b"MEMORY".as_slice(), b"USAGE", key],
        ])
        .await?;
    let kind = KeyKind::parse(&String::from_utf8_lossy(
        meta[0].clone().ok()?.bytes().unwrap_or(b"none"),
    ));
    let ttl_ms = meta[1].int().filter(|t| *t >= 0);
    let memory = meta[2].int().and_then(|m| u64::try_from(m).ok());
    let n = limit.to_string();
    let last = (limit - 1).to_string();
    let len_cmd: Option<&[u8]> = match kind {
        KeyKind::String => Some(b"STRLEN"),
        KeyKind::List => Some(b"LLEN"),
        KeyKind::Set => Some(b"SCARD"),
        KeyKind::ZSet => Some(b"ZCARD"),
        KeyKind::Hash => Some(b"HLEN"),
        KeyKind::Stream => Some(b"XLEN"),
        _ => None,
    };
    let len = match len_cmd {
        Some(cmd) => c.call(&[cmd, key]).await?.ok()?.int().unwrap_or(0).max(0) as u64,
        None => 0,
    };
    let value = match &kind {
        KeyKind::String => {
            let bytes = (limit * 64 - 1).to_string();
            let r = c
                .call(&[b"GETRANGE".as_slice(), key, b"0", bytes.as_bytes()])
                .await?
                .ok()?;
            KeyValue::String(r.bytes().unwrap_or_default().to_vec())
        }
        KeyKind::List => {
            let r = c
                .call(&[b"LRANGE".as_slice(), key, b"0", last.as_bytes()])
                .await?
                .ok()?;
            KeyValue::List(bulk_list(&r))
        }
        KeyKind::Set => KeyValue::Set(
            scan_collection(c, b"SSCAN", key, limit)
                .await?
                .iter()
                .filter_map(|r| r.bytes().map(<[u8]>::to_vec))
                .collect(),
        ),
        KeyKind::Hash => {
            KeyValue::Hash(pairs(&scan_collection(c, b"HSCAN", key, limit * 2).await?))
        }
        KeyKind::ZSet => {
            let r = c
                .call(&[
                    b"ZRANGE".as_slice(),
                    key,
                    b"0",
                    last.as_bytes(),
                    b"WITHSCORES",
                ])
                .await?
                .ok()?;
            KeyValue::ZSet(
                pairs(r.items())
                    .into_iter()
                    .map(|(m, s)| {
                        let s = String::from_utf8_lossy(&s).parse().unwrap_or(f64::NAN);
                        (m, s)
                    })
                    .collect(),
            )
        }
        KeyKind::Stream => {
            let r = c
                .call(&[
                    b"XREVRANGE".as_slice(),
                    key,
                    b"+",
                    b"-",
                    b"COUNT",
                    n.as_bytes(),
                ])
                .await?
                .ok()?;
            KeyValue::Stream(
                r.items()
                    .iter()
                    .filter_map(|e| match e.items() {
                        [id, fields] => Some(StreamEntry {
                            id: String::from_utf8_lossy(id.bytes()?).into_owned(),
                            fields: pairs(fields.items()),
                        }),
                        _ => None,
                    })
                    .collect(),
            )
        }
        KeyKind::Json => {
            let r = c.call(&[b"JSON.GET".as_slice(), key]).await?.ok()?;
            KeyValue::Json(String::from_utf8_lossy(r.bytes().unwrap_or_default()).into_owned())
        }
        KeyKind::Missing => KeyValue::Missing,
        KeyKind::Other(_) => KeyValue::Unsupported,
    };
    Ok(KeyDetails {
        key: key.to_vec(),
        kind,
        ttl_ms,
        len,
        memory,
        value,
    })
}

/// `SSCAN` / `HSCAN` until `max` items or the end (small collections come back in one go).
async fn scan_collection(
    c: &mut RedisClient,
    cmd: &[u8],
    key: &[u8],
    max: usize,
) -> Result<Vec<Reply>> {
    let mut out = Vec::new();
    let mut cursor = b"0".to_vec();
    loop {
        let r = c.call(&[cmd, key, &cursor, b"COUNT", b"500"]).await?.ok()?;
        let [next, items] = r.items() else {
            return Err(DbError::Protocol("unexpected scan reply".into()));
        };
        out.extend(items.items().iter().cloned());
        cursor = next.bytes().unwrap_or(b"0").to_vec();
        if cursor == b"0" || out.len() >= max {
            break;
        }
    }
    // Keep whole pairs for HSCAN.
    out.truncate(max);
    Ok(out)
}

fn bulk_list(r: &Reply) -> Vec<Vec<u8>> {
    r.items()
        .iter()
        .filter_map(|i| i.bytes().map(<[u8]>::to_vec))
        .collect()
}

fn pairs(items: &[Reply]) -> Vec<(Vec<u8>, Vec<u8>)> {
    items
        .as_chunks::<2>()
        .0
        .iter()
        .filter_map(|[k, v]| Some((k.bytes()?.to_vec(), v.bytes()?.to_vec())))
        .collect()
}

/// Apply an edit. With `create`, the key must not exist yet (a new key from the browser).
/// Refused on a read-only connection.
pub async fn edit(c: &mut RedisClient, key: &[u8], edit: &KeyEdit, create: bool) -> Result<()> {
    if c.read_only() {
        return Err(DbError::Unsupported("this connection is read-only".into()));
    }
    if key.is_empty() {
        return Err(DbError::Param("the key name is empty".into()));
    }
    if create {
        let exists = c.call(&[b"EXISTS".as_slice(), key]).await?.ok()?;
        if exists.int().unwrap_or(0) > 0 {
            return Err(DbError::Param("a key with this name already exists".into()));
        }
    }
    let r = c.call(&edit.args(key)).await?.ok()?;
    match edit {
        // RENAMENX answers 0 when the new name is taken.
        KeyEdit::Rename(_) if r.int() == Some(0) => Err(DbError::Param(
            "a key with the new name already exists".into(),
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_commands() {
        let a = |e: KeyEdit| {
            e.args(b"k")
                .into_iter()
                .map(|a| String::from_utf8(a).unwrap())
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(a(KeyEdit::SetString(b"v".to_vec())), "SET k v KEEPTTL");
        assert_eq!(
            a(KeyEdit::ListPush {
                value: b"x".to_vec(),
                head: true
            }),
            "LPUSH k x"
        );
        assert_eq!(a(KeyEdit::ZAdd(b"m".to_vec(), 1.5)), "ZADD k 1.5 m");
        assert_eq!(
            a(KeyEdit::StreamAdd(vec![(b"f".to_vec(), b"v".to_vec())])),
            "XADD k * f v"
        );
        assert_eq!(a(KeyEdit::Expire(Some(60))), "EXPIRE k 60");
        assert_eq!(a(KeyEdit::Expire(None)), "PERSIST k");
        assert_eq!(a(KeyEdit::Rename(b"n".to_vec())), "RENAMENX k n");
        assert_eq!(a(KeyEdit::Delete), "UNLINK k");
    }

    #[test]
    fn truncation() {
        let d = KeyDetails {
            key: b"k".to_vec(),
            kind: KeyKind::List,
            ttl_ms: None,
            len: 3,
            memory: None,
            value: KeyValue::List(vec![b"a".to_vec()]),
        };
        assert!(d.truncated());
        assert!(!KeyDetails { len: 1, ..d }.truncated());
    }

    #[test]
    fn kinds_parse() {
        assert_eq!(KeyKind::parse("zset"), KeyKind::ZSet);
        assert_eq!(KeyKind::parse("ReJSON-RL"), KeyKind::Json);
        assert_eq!(
            KeyKind::parse("TSDB-TYPE"),
            KeyKind::Other("TSDB-TYPE".into())
        );
    }
}
