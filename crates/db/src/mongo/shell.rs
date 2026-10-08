//! The mongosh subset a MongoDB statement is written in.
//!
//! A statement is one of
//!
//! - `show dbs` / `show databases`, `show collections`, `use <db>`;
//! - a command document, run against the current database (`{ ping: 1 }`);
//! - a call chain on `db`: `db.orders.find({ status: "A" }).sort({ total: -1 }).limit(20)`,
//!   `db.getCollection("x").aggregate([...])`, `db.getSiblingDB("shop").orders.countDocuments()`,
//!   `db.runCommand({...})`, `db.adminCommand({...})`, `db.stats()`.
//!
//! Arguments are relaxed JSON as mongosh writes it: unquoted keys, single or double
//! quotes, trailing commas, comments, `/regex/flags`, and the shell constructors
//! (`ObjectId`, `ISODate`, `new Date`, `NumberLong`, `NumberInt`, `NumberDecimal`, `UUID`,
//! `Timestamp`, `MinKey`, `MaxKey`). Every statement becomes one server command; nothing
//! is evaluated as JavaScript.

use std::str::FromStr;

use mongodb::bson::oid::ObjectId;
use mongodb::bson::spec::BinarySubtype;
use mongodb::bson::{Binary, Bson, DateTime, Decimal128, Document, Regex, Timestamp, doc};

/// A parse failure with the byte offset it was found at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// What went wrong.
    pub message: String,
    /// Byte offset into the statement.
    pub offset: usize,
}

type PResult<T> = Result<T, ParseError>;

/// How the server's answer to a [`Op::Command`] is shown.
#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    /// A cursor command (`find`, `aggregate`, `listIndexes`, …): documents stream as rows.
    Cursor,
    /// The reply document as one row.
    Reply,
    /// `count`: the `n` field as a `count` column.
    Count,
    /// `distinct`: one row per value.
    Distinct,
    /// `listDatabases`: one row per database.
    Databases,
    /// A write (`insert`, `update`, `delete`): affected counts, no rows.
    Write,
}

/// One statement, ready to run.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// `use <db>`: make another database current.
    Use(String),
    /// A server command.
    Command {
        /// Database to run it in (`None`: the current one).
        db: Option<String>,
        /// The command document.
        command: Document,
        /// How to show the reply.
        shape: Shape,
        /// Ids given to inserted documents that had none (reported after the insert).
        inserted: Vec<Bson>,
    },
}

/// What a statement does, for read-only connections and Production confirmations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Reads only.
    Read,
    /// Writes; `destructive` names what needs confirmation on Production.
    Write {
        /// `(kind, object)`: `drop collection`, `drop database`, `delete all`, `update all`.
        destructive: Option<(Destructive, String)>,
    },
}

/// A write that needs confirmation on Production.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Destructive {
    /// `db.x.drop()`.
    DropCollection,
    /// `db.dropDatabase()`.
    DropDatabase,
    /// `deleteMany({})`: every document.
    DeleteAll,
    /// `updateMany({}, …)`: every document.
    UpdateAll,
}

/// Commands that never write (first key of a command document, lower-cased).
const READ_COMMANDS: &[&str] = &[
    "aggregate",
    "buildinfo",
    "collstats",
    "connectionstatus",
    "count",
    "datasize",
    "dbstats",
    "distinct",
    "explain",
    "find",
    "getcmdlineopts",
    "getlog",
    "getmore",
    "getparameter",
    "hello",
    "hostinfo",
    "ismaster",
    "listcollections",
    "listcommands",
    "listdatabases",
    "listindexes",
    "ping",
    "replsetgetstatus",
    "serverstatus",
    "top",
    "usersinfo",
    "rolesinfo",
    "validate",
    "whatsmyuri",
];

impl Op {
    /// Whether the statement reads only, and what a destructive write would hit.
    pub fn effect(&self) -> Effect {
        let Op::Command { command, .. } = self else {
            return Effect::Read;
        };
        let Some((name, value)) = command.iter().next() else {
            return Effect::Read;
        };
        let name = name.to_ascii_lowercase();
        let target = value.as_str().unwrap_or_default().to_owned();
        match name.as_str() {
            "aggregate" => {
                let writes = command
                    .get_array("pipeline")
                    .map(|p| {
                        p.iter().any(|s| {
                            s.as_document()
                                .is_some_and(|d| d.contains_key("$out") || d.contains_key("$merge"))
                        })
                    })
                    .unwrap_or(false);
                if writes {
                    Effect::Write { destructive: None }
                } else {
                    Effect::Read
                }
            }
            n if READ_COMMANDS.contains(&n) => Effect::Read,
            "drop" => Effect::Write {
                destructive: Some((Destructive::DropCollection, target)),
            },
            "dropdatabase" => Effect::Write {
                destructive: Some((Destructive::DropDatabase, String::new())),
            },
            "delete" => {
                let all = command.get_array("deletes").is_ok_and(|ds| {
                    ds.iter().any(|d| {
                        d.as_document()
                            .and_then(|d| d.get_document("q").ok())
                            .is_some_and(Document::is_empty)
                    })
                });
                Effect::Write {
                    destructive: all.then_some((Destructive::DeleteAll, target)),
                }
            }
            "update" => {
                let all = command.get_array("updates").is_ok_and(|us| {
                    us.iter().any(|u| {
                        u.as_document().is_some_and(|u| {
                            u.get_bool("multi").unwrap_or(false)
                                && u.get_document("q").is_ok_and(Document::is_empty)
                        })
                    })
                });
                Effect::Write {
                    destructive: all.then_some((Destructive::UpdateAll, target)),
                }
            }
            _ => Effect::Write { destructive: None },
        }
    }
}

/// Parse one statement.
pub fn parse(text: &str) -> PResult<Op> {
    let mut p = Parser { s: text, i: 0 };
    p.ws();
    let op = p.statement()?;
    p.ws();
    while p.eat(b';') {
        p.ws();
    }
    if p.i < p.s.len() {
        return Err(p.err("Unexpected text after the statement"));
    }
    Ok(op)
}

/// Parse a relaxed-JSON document (a filter typed in the data view).
pub fn parse_document(text: &str) -> PResult<Document> {
    let mut p = Parser { s: text, i: 0 };
    p.ws();
    if p.i == p.s.len() {
        return Ok(Document::new());
    }
    let d = p.document()?;
    p.ws();
    if p.i < p.s.len() {
        return Err(p.err("Unexpected text after the document"));
    }
    Ok(d)
}

/// One link of a `db…` call chain.
#[derive(Debug)]
enum Link {
    Prop(String, usize),
    Call(String, Vec<Bson>, usize),
}

struct Parser<'a> {
    s: &'a str,
    i: usize,
}

impl Parser<'_> {
    fn err(&self, message: impl Into<String>) -> ParseError {
        ParseError {
            message: message.into(),
            offset: self.i,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8) -> PResult<()> {
        self.ws();
        if self.eat(c) {
            Ok(())
        } else {
            Err(self.err(format!("Expected `{}`", c as char)))
        }
    }

    /// Skip whitespace and `//` / `/* */` comments.
    fn ws(&mut self) {
        let b = self.s.as_bytes();
        loop {
            while self.i < b.len() && b[self.i].is_ascii_whitespace() {
                self.i += 1;
            }
            if b[self.i..].starts_with(b"//") {
                self.i = self.s[self.i..].find('\n').map_or(b.len(), |p| self.i + p);
            } else if b[self.i..].starts_with(b"/*") {
                self.i = self.s[self.i + 2..]
                    .find("*/")
                    .map_or(b.len(), |p| self.i + 2 + p + 2);
            } else {
                return;
            }
        }
    }

    fn ident(&mut self) -> Option<String> {
        let b = self.s.as_bytes();
        let start = self.i;
        while self.i < b.len()
            && (b[self.i].is_ascii_alphanumeric()
                || matches!(b[self.i], b'_' | b'$')
                || b[self.i] >= 0x80)
        {
            self.i += 1;
        }
        (self.i > start).then(|| self.s[start..self.i].to_owned())
    }

    /// A database or collection name after `use`: anything up to whitespace or `;`.
    fn word(&mut self) -> Option<String> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() || c == b';' {
                break;
            }
            self.i += 1;
        }
        (self.i > start).then(|| self.s[start..self.i].to_owned())
    }

    fn statement(&mut self) -> PResult<Op> {
        if self.peek() == Some(b'{') {
            let command = self.document()?;
            return command_op(None, command, self.i);
        }
        let at = self.i;
        let Some(head) = self.ident() else {
            return Err(self.err(
                "Expected db.<collection>.<method>(…), show dbs, use <database> or a command document",
            ));
        };
        match head.as_str() {
            "show" => {
                self.ws();
                let what = self.word().unwrap_or_default();
                match what.as_str() {
                    "dbs" | "databases" => Ok(command(None, doc! { "listDatabases": 1 }, Shape::Databases)),
                    "collections" | "tables" => Ok(command(
                        None,
                        doc! { "listCollections": 1, "nameOnly": true, "authorizedCollections": true },
                        Shape::Cursor,
                    )),
                    _ => Err(self.err("Expected `show dbs` or `show collections`")),
                }
            }
            "use" => {
                self.ws();
                let db = self
                    .word()
                    .ok_or_else(|| self.err("Expected a database name"))?;
                Ok(Op::Use(unquote(&db)))
            }
            "db" => {
                let links = self.chain()?;
                interpret(links, self.i)
            }
            _ => Err(ParseError {
                message: format!(
                    "Unknown statement `{head}`: start with db., show, use, or a command document"
                ),
                offset: at,
            }),
        }
    }

    /// `.name`, `["name"]` and `(args)` links after `db`.
    fn chain(&mut self) -> PResult<Vec<Link>> {
        let mut links: Vec<Link> = Vec::new();
        loop {
            self.ws();
            let at = self.i;
            if self.eat(b'.') {
                self.ws();
                let name = self
                    .ident()
                    .ok_or_else(|| self.err("Expected a name after `.`"))?;
                links.push(Link::Prop(name, at));
            } else if self.eat(b'[') {
                self.ws();
                let name = match self.value()? {
                    Bson::String(s) => s,
                    _ => return Err(self.err("Expected a collection name in quotes")),
                };
                self.expect(b']')?;
                links.push(Link::Prop(name, at));
            } else if self.eat(b'(') {
                let args = self.args()?;
                match links.pop() {
                    Some(Link::Prop(name, at)) => links.push(Link::Call(name, args, at)),
                    _ => return Err(self.err("Unexpected `(`")),
                }
            } else {
                return Ok(links);
            }
        }
    }

    /// Arguments up to the closing `)` (the `(` is consumed).
    fn args(&mut self) -> PResult<Vec<Bson>> {
        let mut out = Vec::new();
        loop {
            self.ws();
            if self.eat(b')') {
                return Ok(out);
            }
            out.push(self.value()?);
            self.ws();
            if !self.eat(b',') {
                self.expect(b')')?;
                return Ok(out);
            }
        }
    }

    fn document(&mut self) -> PResult<Document> {
        self.expect(b'{')?;
        let mut d = Document::new();
        loop {
            self.ws();
            if self.eat(b'}') {
                return Ok(d);
            }
            let key = match self.peek() {
                Some(b'"' | b'\'') => self.string()?,
                _ => {
                    let start = self.i;
                    let b = self.s.as_bytes();
                    while self.i < b.len()
                        && (b[self.i].is_ascii_alphanumeric()
                            || matches!(b[self.i], b'_' | b'$' | b'.')
                            || b[self.i] >= 0x80)
                    {
                        self.i += 1;
                    }
                    if self.i == start {
                        return Err(self.err("Expected a field name"));
                    }
                    self.s[start..self.i].to_owned()
                }
            };
            self.expect(b':')?;
            self.ws();
            let v = self.value()?;
            d.insert(key, v);
            self.ws();
            if !self.eat(b',') {
                self.expect(b'}')?;
                return Ok(d);
            }
        }
    }

    fn array(&mut self) -> PResult<Vec<Bson>> {
        self.expect(b'[')?;
        let mut out = Vec::new();
        loop {
            self.ws();
            if self.eat(b']') {
                return Ok(out);
            }
            out.push(self.value()?);
            self.ws();
            if !self.eat(b',') {
                self.expect(b']')?;
                return Ok(out);
            }
        }
    }

    fn string(&mut self) -> PResult<String> {
        let quote = self.peek().ok_or_else(|| self.err("Expected a string"))?;
        self.i += 1;
        let mut out = String::new();
        let mut chars = self.s[self.i..].char_indices();
        while let Some((off, c)) = chars.next() {
            match c {
                c if c as u32 == u32::from(quote) => {
                    self.i += off + 1;
                    return Ok(out);
                }
                '\\' => {
                    let Some((_, e)) = chars.next() else { break };
                    match e {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        '0' => out.push('\0'),
                        'u' => {
                            let hex: String = (0..4).filter_map(|_| chars.next()).map(|(_, h)| h).collect();
                            let ch = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32);
                            match ch {
                                Some(ch) => out.push(ch),
                                None => {
                                    return Err(self.err("Invalid \\u escape in a string"));
                                }
                            }
                        }
                        other => out.push(other),
                    }
                }
                c => out.push(c),
            }
        }
        Err(self.err("Unterminated string"))
    }

    fn number(&mut self) -> PResult<Bson> {
        let start = self.i;
        let b = self.s.as_bytes();
        if matches!(self.peek(), Some(b'-' | b'+')) {
            self.i += 1;
        }
        if self.s[self.i..].starts_with("Infinity") {
            self.i += "Infinity".len();
            return Ok(Bson::Double(if b[start] == b'-' {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }));
        }
        let mut float = false;
        while self.i < b.len() {
            match b[self.i] {
                b'0'..=b'9' => {}
                b'.' => float = true,
                b'e' | b'E' => {
                    float = true;
                    if matches!(b.get(self.i + 1), Some(b'-' | b'+')) {
                        self.i += 1;
                    }
                }
                _ => break,
            }
            self.i += 1;
        }
        let text = &self.s[start..self.i];
        if !float {
            if let Ok(n) = text.parse::<i32>() {
                return Ok(Bson::Int32(n));
            }
            if let Ok(n) = text.parse::<i64>() {
                return Ok(Bson::Int64(n));
            }
        }
        text.parse::<f64>().map(Bson::Double).map_err(|_| ParseError {
            message: format!("Invalid number `{text}`"),
            offset: start,
        })
    }

    fn regex(&mut self) -> PResult<Bson> {
        self.i += 1; // opening '/'
        let b = self.s.as_bytes();
        let start = self.i;
        let mut class = false;
        while self.i < b.len() {
            match b[self.i] {
                b'\\' => self.i += 1,
                b'[' => class = true,
                b']' => class = false,
                b'/' if !class => break,
                b'\n' => break,
                _ => {}
            }
            self.i += 1;
        }
        if self.peek() != Some(b'/') {
            return Err(self.err("Unterminated regular expression"));
        }
        let pattern = self.s[start..self.i].to_owned();
        self.i += 1;
        let flags_start = self.i;
        while matches!(self.peek(), Some(c) if c.is_ascii_alphabetic()) {
            self.i += 1;
        }
        let mut options: Vec<char> = self.s[flags_start..self.i].chars().collect();
        options.sort_unstable();
        Ok(Bson::RegularExpression(Regex {
            pattern,
            options: options.into_iter().collect(),
        }))
    }

    fn value(&mut self) -> PResult<Bson> {
        self.ws();
        match self.peek() {
            None => Err(self.err("Expected a value")),
            Some(b'{') => self.document().map(Bson::Document),
            Some(b'[') => self.array().map(Bson::Array),
            Some(b'"' | b'\'') => self.string().map(Bson::String),
            Some(b'/') => self.regex(),
            Some(c) if c.is_ascii_digit() || c == b'-' || c == b'+' || c == b'.' => self.number(),
            Some(_) => {
                let at = self.i;
                let name = self.ident().ok_or_else(|| self.err("Expected a value"))?;
                let name = if name == "new" {
                    self.ws();
                    self.ident().ok_or_else(|| self.err("Expected a constructor after `new`"))?
                } else {
                    name
                };
                match name.as_str() {
                    "true" => return Ok(Bson::Boolean(true)),
                    "false" => return Ok(Bson::Boolean(false)),
                    "null" | "undefined" => return Ok(Bson::Null),
                    "NaN" => return Ok(Bson::Double(f64::NAN)),
                    "Infinity" => return Ok(Bson::Double(f64::INFINITY)),
                    "MinKey" | "MaxKey" if !matches!(self.peek_after_ws(), Some(b'(')) => {
                        return Ok(if name == "MinKey" {
                            Bson::MinKey
                        } else {
                            Bson::MaxKey
                        });
                    }
                    _ => {}
                }
                self.ws();
                if !self.eat(b'(') {
                    return Err(ParseError {
                        message: format!(
                            "`{name}` is not a value: variables and JavaScript are not supported"
                        ),
                        offset: at,
                    });
                }
                let args = self.args()?;
                constructor(&name, args).map_err(|message| ParseError {
                    message,
                    offset: at,
                })
            }
        }
    }

    fn peek_after_ws(&mut self) -> Option<u8> {
        let save = self.i;
        self.ws();
        let c = self.peek();
        self.i = save;
        c
    }
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    for q in ['"', '\''] {
        if let Some(inner) = t.strip_prefix(q).and_then(|t| t.strip_suffix(q)) {
            return inner.to_owned();
        }
    }
    t.to_owned()
}

fn arg_str(args: &[Bson], ix: usize) -> Option<&str> {
    args.get(ix).and_then(Bson::as_str)
}

/// A date from ISO-8601 text (`2024-05-01`, `2024-05-01T10:00`, with or without zone).
fn parse_date(s: &str) -> Option<DateTime> {
    let s = s.trim();
    let candidates = [
        s.to_owned(),
        format!("{s}Z"),
        format!("{s}:00Z"),
        format!("{s}T00:00:00Z"),
    ];
    candidates
        .iter()
        .find_map(|c| DateTime::parse_rfc3339_str(c).ok())
}

fn int_arg(v: Option<&Bson>) -> Option<i64> {
    match v? {
        Bson::Int32(n) => Some(i64::from(*n)),
        Bson::Int64(n) => Some(*n),
        Bson::Double(f) if f.fract() == 0.0 => Some(*f as i64),
        Bson::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `ObjectId("…")`, `ISODate("…")`, `NumberLong(…)` and friends.
fn constructor(name: &str, args: Vec<Bson>) -> Result<Bson, String> {
    Ok(match name {
        "ObjectId" => match arg_str(&args, 0) {
            Some(hex) => Bson::ObjectId(
                ObjectId::parse_str(hex.trim()).map_err(|_| format!("Invalid ObjectId `{hex}`"))?,
            ),
            None => Bson::ObjectId(ObjectId::new()),
        },
        "ISODate" | "Date" => match args.first() {
            None => Bson::DateTime(DateTime::now()),
            Some(Bson::String(s)) => {
                Bson::DateTime(parse_date(s).ok_or_else(|| format!("Invalid date `{s}`"))?)
            }
            Some(v) => Bson::DateTime(DateTime::from_millis(
                int_arg(Some(v)).ok_or("Date takes ISO text or milliseconds")?,
            )),
        },
        "NumberLong" => Bson::Int64(int_arg(args.first()).ok_or("NumberLong takes an integer")?),
        "NumberInt" => Bson::Int32(
            int_arg(args.first())
                .and_then(|n| i32::try_from(n).ok())
                .ok_or("NumberInt takes a 32-bit integer")?,
        ),
        "NumberDecimal" | "Decimal128" => {
            let text = match args.first() {
                Some(Bson::String(s)) => s.clone(),
                Some(Bson::Int32(n)) => n.to_string(),
                Some(Bson::Int64(n)) => n.to_string(),
                Some(Bson::Double(f)) => f.to_string(),
                _ => return Err("NumberDecimal takes a number in quotes".into()),
            };
            Bson::Decimal128(
                Decimal128::from_str(&text).map_err(|_| format!("Invalid decimal `{text}`"))?,
            )
        }
        "UUID" => {
            let text = arg_str(&args, 0).ok_or("UUID takes a string")?;
            let hex: String = text.chars().filter(|c| *c != '-').collect();
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|i| hex.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
                .collect::<Option<Vec<u8>>>()
                .filter(|b| b.len() == 16)
                .ok_or_else(|| format!("Invalid UUID `{text}`"))?;
            Bson::Binary(Binary {
                subtype: BinarySubtype::Uuid,
                bytes,
            })
        }
        "Timestamp" => Bson::Timestamp(Timestamp {
            time: int_arg(args.first())
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0),
            increment: int_arg(args.get(1))
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0),
        }),
        "MinKey" => Bson::MinKey,
        "MaxKey" => Bson::MaxKey,
        "RegExp" => Bson::RegularExpression(Regex {
            pattern: arg_str(&args, 0).ok_or("RegExp takes a pattern")?.to_owned(),
            options: arg_str(&args, 1).unwrap_or_default().to_owned(),
        }),
        other => return Err(format!("`{other}(…)` is not supported")),
    })
}

fn command(db: Option<String>, command: Document, shape: Shape) -> Op {
    Op::Command {
        db,
        command,
        shape,
        inserted: Vec::new(),
    }
}

/// A command document typed on its own or passed to `runCommand`.
fn command_op(db: Option<String>, cmd: Document, at: usize) -> PResult<Op> {
    let Some(first) = cmd.keys().next() else {
        return Err(ParseError {
            message: "The command document is empty".into(),
            offset: at,
        });
    };
    let shape = match first.to_ascii_lowercase().as_str() {
        "find" | "aggregate" | "listcollections" | "listindexes" => Shape::Cursor,
        "listdatabases" => Shape::Databases,
        _ => Shape::Reply,
    };
    Ok(command(db, cmd, shape))
}

fn doc_arg(args: &[Bson], ix: usize, what: &str, at: usize) -> PResult<Document> {
    match args.get(ix) {
        None | Some(Bson::Null) => Ok(Document::new()),
        Some(Bson::Document(d)) => Ok(d.clone()),
        Some(_) => Err(ParseError {
            message: format!("{what} must be a document"),
            offset: at,
        }),
    }
}

/// A find under construction: `find(filter, projection)` plus cursor modifiers.
struct Find {
    filter: Document,
    projection: Option<Document>,
    sort: Option<Document>,
    skip: Option<i64>,
    limit: Option<i64>,
    one: bool,
}

impl Find {
    fn command(self, collection: &str) -> Document {
        let mut c = doc! { "find": collection, "filter": self.filter };
        if let Some(p) = self.projection.filter(|p| !p.is_empty()) {
            c.insert("projection", p);
        }
        if let Some(s) = self.sort.filter(|s| !s.is_empty()) {
            c.insert("sort", s);
        }
        if let Some(n) = self.skip.filter(|n| *n > 0) {
            c.insert("skip", n);
        }
        if self.one {
            c.insert("limit", 1i64);
            c.insert("singleBatch", true);
        } else if let Some(n) = self.limit.filter(|n| *n != 0) {
            c.insert("limit", n.abs());
            if n < 0 {
                c.insert("singleBatch", true);
            }
        }
        c
    }
}

/// Generated index name, as the shell makes it: `field_1_other_-1`.
fn index_name(keys: &Document) -> String {
    keys.iter()
        .map(|(k, v)| {
            let dir = match v {
                Bson::String(s) => s.clone(),
                Bson::Int32(n) => n.to_string(),
                Bson::Int64(n) => n.to_string(),
                Bson::Double(f) => (*f as i64).to_string(),
                other => other.to_string(),
            };
            format!("{k}_{dir}")
        })
        .collect::<Vec<_>>()
        .join("_")
}

/// Turn a `db…` chain into one command.
fn interpret(links: Vec<Link>, end: usize) -> PResult<Op> {
    let mut it = links.into_iter().peekable();
    let mut db: Option<String> = None;
    let bad = |message: String, offset: usize| ParseError { message, offset };
    // Database selectors and database-level methods.
    let collection = loop {
        match it.next() {
            None => {
                return Err(bad(
                    "Expected a collection or method after `db`: db.<collection>.find()".into(),
                    end,
                ));
            }
            Some(Link::Call(name, args, at)) => match name.as_str() {
                "getSiblingDB" => {
                    db = Some(
                        arg_str(&args, 0)
                            .ok_or_else(|| bad("getSiblingDB takes a database name".into(), at))?
                            .to_owned(),
                    );
                }
                "getCollection" => {
                    break arg_str(&args, 0)
                        .ok_or_else(|| bad("getCollection takes a collection name".into(), at))?
                        .to_owned();
                }
                "runCommand" | "adminCommand" => {
                    let cmd = match args.first() {
                        Some(Bson::Document(d)) => d.clone(),
                        Some(Bson::String(s)) => doc! { s.clone(): 1 },
                        _ => return Err(bad(format!("{name} takes a command document"), at)),
                    };
                    let target = if name == "adminCommand" {
                        Some("admin".to_owned())
                    } else {
                        db
                    };
                    return finish(command_op(target, cmd, at)?, it);
                }
                "getCollectionNames" | "getCollectionInfos" => {
                    let mut c = doc! { "listCollections": 1, "authorizedCollections": true };
                    if name == "getCollectionNames" {
                        c.insert("nameOnly", true);
                    }
                    if let Some(Bson::Document(f)) = args.first() {
                        c.insert("filter", f.clone());
                    }
                    return finish(command(db, c, Shape::Cursor), it);
                }
                "stats" => return finish(command(db, doc! { "dbStats": 1 }, Shape::Reply), it),
                "version" | "serverBuildInfo" => {
                    return finish(command(db, doc! { "buildInfo": 1 }, Shape::Reply), it);
                }
                "serverStatus" => {
                    return finish(command(db, doc! { "serverStatus": 1 }, Shape::Reply), it);
                }
                "hello" | "isMaster" => {
                    return finish(command(db, doc! { "hello": 1 }, Shape::Reply), it);
                }
                "dropDatabase" => {
                    return finish(command(db, doc! { "dropDatabase": 1 }, Shape::Reply), it);
                }
                "createCollection" => {
                    let n = arg_str(&args, 0)
                        .ok_or_else(|| bad("createCollection takes a name".into(), at))?;
                    let mut c = doc! { "create": n };
                    c.extend(doc_arg(&args, 1, "The options", at)?);
                    return finish(command(db, c, Shape::Reply), it);
                }
                "createView" => {
                    let n = arg_str(&args, 0)
                        .ok_or_else(|| bad("createView takes a name".into(), at))?;
                    let on = arg_str(&args, 1)
                        .ok_or_else(|| bad("createView takes a source collection".into(), at))?;
                    let pipeline = match args.get(2) {
                        Some(Bson::Array(a)) => a.clone(),
                        _ => Vec::new(),
                    };
                    return finish(
                        command(
                            db,
                            doc! { "create": n, "viewOn": on, "pipeline": pipeline },
                            Shape::Reply,
                        ),
                        it,
                    );
                }
                other => {
                    return Err(bad(format!("db.{other}() is not supported"), at));
                }
            },
            Some(Link::Prop(name, _)) => break name,
        }
    };
    let Some(Link::Call(method, args, at)) = it.next() else {
        return Err(bad(
            format!("Call a method on {collection}: find(), aggregate(), countDocuments(), …"),
            end,
        ));
    };
    let c = collection.as_str();
    let op = match method.as_str() {
        "find" | "findOne" => {
            let mut f = Find {
                filter: doc_arg(&args, 0, "The filter", at)?,
                projection: Some(doc_arg(&args, 1, "The projection", at)?),
                sort: None,
                skip: None,
                limit: None,
                one: method == "findOne",
            };
            // Cursor modifiers.
            let mut count = false;
            let mut explain: Option<String> = None;
            for link in it.by_ref() {
                let Link::Call(m, a, at) = link else {
                    let Link::Prop(p, at) = link else {
                        unreachable!()
                    };
                    return Err(bad(format!("Unexpected `.{p}`"), at));
                };
                match m.as_str() {
                    "sort" => f.sort = Some(doc_arg(&a, 0, "The sort", at)?),
                    "projection" | "project" => {
                        f.projection = Some(doc_arg(&a, 0, "The projection", at)?);
                    }
                    "limit" => f.limit = int_arg(a.first()),
                    "skip" => f.skip = int_arg(a.first()),
                    "count" | "size" => count = true,
                    "explain" => {
                        explain = Some(arg_str(&a, 0).unwrap_or("queryPlanner").to_owned());
                    }
                    "pretty" | "toArray" | "batchSize" | "maxTimeMS" | "hint" | "comment" => {}
                    other => return Err(bad(format!("Cursor method {other}() is not supported"), at)),
                }
            }
            if count {
                let mut cmd = doc! { "count": c, "query": f.filter };
                if let Some(n) = f.skip.filter(|n| *n > 0) {
                    cmd.insert("skip", n);
                }
                if let Some(n) = f.limit.filter(|n| *n != 0) {
                    cmd.insert("limit", n.abs());
                }
                command(db, cmd, Shape::Count)
            } else if let Some(verbosity) = explain {
                command(
                    db,
                    doc! { "explain": f.command(c), "verbosity": verbosity },
                    Shape::Reply,
                )
            } else {
                command(db, f.command(c), Shape::Cursor)
            }
        }
        "aggregate" => {
            let pipeline = match args.first() {
                Some(Bson::Array(a)) => a.clone(),
                Some(Bson::Document(_)) => args.iter().take_while(|a| a.as_document().is_some()).cloned().collect(),
                None => Vec::new(),
                _ => return Err(bad("aggregate takes a pipeline array".into(), at)),
            };
            let mut cmd = doc! { "aggregate": c, "pipeline": pipeline, "cursor": {} };
            if let Some(Bson::Document(o)) = args.get(1).filter(|_| args.first().is_some_and(|a| a.as_array().is_some())) {
                for (k, v) in o {
                    if k != "cursor" {
                        cmd.insert(k.clone(), v.clone());
                    }
                }
            }
            let mut explain = None;
            for link in it.by_ref() {
                match link {
                    Link::Call(m, a, _) if m == "explain" => {
                        explain = Some(arg_str(&a, 0).unwrap_or("queryPlanner").to_owned());
                    }
                    Link::Call(m, _, _) if m == "toArray" || m == "pretty" => {}
                    Link::Call(m, _, at) | Link::Prop(m, at) => {
                        return Err(bad(format!("Unexpected `.{m}` after aggregate()"), at));
                    }
                }
            }
            match explain {
                Some(v) => command(db, doc! { "explain": cmd, "verbosity": v }, Shape::Reply),
                None => command(db, cmd, Shape::Cursor),
            }
        }
        "countDocuments" | "count" => {
            let mut cmd = doc! { "count": c, "query": doc_arg(&args, 0, "The filter", at)? };
            if let Ok(o) = doc_arg(&args, 1, "The options", at) {
                for k in ["limit", "skip", "hint"] {
                    if let Some(v) = o.get(k) {
                        cmd.insert(k, v.clone());
                    }
                }
            }
            command(db, cmd, Shape::Count)
        }
        "estimatedDocumentCount" => command(db, doc! { "count": c }, Shape::Count),
        "distinct" => {
            let key = arg_str(&args, 0).ok_or_else(|| bad("distinct takes a field name".into(), at))?;
            command(
                db,
                doc! { "distinct": c, "key": key, "query": doc_arg(&args, 1, "The filter", at)? },
                Shape::Distinct,
            )
        }
        "getIndexes" | "getIndices" => command(db, doc! { "listIndexes": c }, Shape::Cursor),
        "stats" => command(db, doc! { "collStats": c }, Shape::Reply),
        "insertOne" | "insert" | "insertMany" => {
            let docs: Vec<Document> = match args.first() {
                Some(Bson::Document(d)) if method != "insertMany" => vec![d.clone()],
                Some(Bson::Array(a)) => a
                    .iter()
                    .map(|d| d.as_document().cloned())
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| bad("Every inserted value must be a document".into(), at))?,
                _ => return Err(bad(format!("{method} takes a document"), at)),
            };
            let mut inserted = Vec::new();
            let docs: Vec<Document> = docs
                .into_iter()
                .map(|mut d| {
                    if !d.contains_key("_id") {
                        let mut with_id = doc! { "_id": ObjectId::new() };
                        with_id.extend(d);
                        d = with_id;
                    }
                    inserted.push(d.get("_id").cloned().unwrap_or(Bson::Null));
                    d
                })
                .collect();
            return Ok(Op::Command {
                db,
                command: doc! { "insert": c, "documents": docs },
                shape: Shape::Write,
                inserted,
            });
        }
        "updateOne" | "updateMany" | "replaceOne" => {
            let q = doc_arg(&args, 0, "The filter", at)?;
            let u = match args.get(1) {
                Some(b @ (Bson::Document(_) | Bson::Array(_))) => b.clone(),
                _ => return Err(bad(format!("{method} takes a filter and an update"), at)),
            };
            if method == "replaceOne"
                && u.as_document().is_some_and(|d| d.keys().any(|k| k.starts_with('$')))
            {
                return Err(bad("A replacement cannot contain update operators".into(), at));
            }
            if method != "replaceOne"
                && u.as_document()
                    .is_some_and(|d| d.keys().next().is_some_and(|k| !k.starts_with('$')))
            {
                return Err(bad(
                    "An update needs operators such as $set; use replaceOne to replace a document"
                        .into(),
                    at,
                ));
            }
            let o = doc_arg(&args, 2, "The options", at)?;
            let mut stmt = doc! {
                "q": q,
                "u": u,
                "multi": method == "updateMany",
                "upsert": o.get_bool("upsert").unwrap_or(false),
            };
            if let Some(f) = o.get("arrayFilters") {
                stmt.insert("arrayFilters", f.clone());
            }
            command(db, doc! { "update": c, "updates": [stmt] }, Shape::Write)
        }
        "deleteOne" | "deleteMany" | "remove" => {
            let q = doc_arg(&args, 0, "The filter", at)?;
            let limit = i32::from(method == "deleteOne");
            command(db, doc! { "delete": c, "deletes": [{ "q": q, "limit": limit }] }, Shape::Write)
        }
        "createIndex" | "ensureIndex" => {
            let keys = doc_arg(&args, 0, "The index keys", at)?;
            if keys.is_empty() {
                return Err(bad("createIndex takes the index keys".into(), at));
            }
            let mut index = doc! { "key": keys.clone() };
            let o = doc_arg(&args, 1, "The options", at)?;
            if !o.contains_key("name") {
                index.insert("name", index_name(&keys));
            }
            index.extend(o);
            command(db, doc! { "createIndexes": c, "indexes": [index] }, Shape::Reply)
        }
        "dropIndex" => {
            let index = args
                .first()
                .cloned()
                .ok_or_else(|| bad("dropIndex takes an index name or keys".into(), at))?;
            command(db, doc! { "dropIndexes": c, "index": index }, Shape::Reply)
        }
        "drop" => command(db, doc! { "drop": c }, Shape::Reply),
        "renameCollection" => {
            let to = arg_str(&args, 0).ok_or_else(|| bad("renameCollection takes a new name".into(), at))?;
            let from_db = db.clone().unwrap_or_default();
            // The command runs in admin with full names; the session fills in the
            // current database for `{db}`.
            command(
                Some("admin".into()),
                doc! { "renameCollection": format!("{from_db}.{c}"), "to": format!("{from_db}.{to}") },
                Shape::Reply,
            )
        }
        other => return Err(bad(format!("{collection}.{other}() is not supported"), at)),
    };
    finish(op, it)
}

/// Nothing may follow a finished statement except `.pretty()` / `.toArray()`.
fn finish(op: Op, rest: impl Iterator<Item = Link>) -> PResult<Op> {
    for link in rest {
        match link {
            Link::Call(m, _, _) if m == "pretty" || m == "toArray" => {}
            Link::Call(m, _, at) | Link::Prop(m, at) => {
                return Err(ParseError {
                    message: format!("Unexpected `.{m}`"),
                    offset: at,
                });
            }
        }
    }
    Ok(op)
}

/// Statement boundaries in a script: `;`, or a line break outside brackets when the next
/// line does not continue the chain (does not start with `.`) and the line does not end
/// in an operator that needs more (`.` or `,`). Returns `(start, end)` byte ranges with
/// surrounding whitespace trimmed.
pub fn split(script: &str) -> Vec<(usize, usize)> {
    let b = script.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0;
    let mut i = 0;
    let mut last_sig: u8 = 0;
    let push = |out: &mut Vec<(usize, usize)>, s: usize, e: usize| {
        let text = &script[s..e];
        let lead = text.len() - text.trim_start().len();
        let trail = text.len() - text.trim_end().len();
        if lead < text.len() {
            out.push((s + lead, e - trail));
        }
    };
    while i < n {
        let c = b[i];
        match c {
            b'"' | b'\'' => {
                i += 1;
                while i < n && b[i] != c && b[i] != b'\n' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                last_sig = c;
            }
            b'/' if i + 1 < n && b[i + 1] == b'/' => {
                i = script[i..].find('\n').map_or(n, |p| i + p);
                continue;
            }
            b'/' if i + 1 < n && b[i + 1] == b'*' => {
                i = script[i + 2..].find("*/").map_or(n, |p| i + 2 + p + 2);
                continue;
            }
            b'{' | b'[' | b'(' => {
                depth += 1;
                last_sig = c;
            }
            b'}' | b']' | b')' => {
                depth = (depth - 1).max(0);
                last_sig = c;
            }
            b';' if depth == 0 => {
                push(&mut out, start, i);
                start = i + 1;
                last_sig = 0;
            }
            b'\n' if depth == 0 && last_sig != 0 && !matches!(last_sig, b'.' | b',' | b':') => {
                // Look at the next non-blank, non-comment character.
                let mut j = i + 1;
                loop {
                    while j < n && b[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j + 1 < n && b[j] == b'/' && b[j + 1] == b'/' {
                        j = script[j..].find('\n').map_or(n, |p| j + p);
                        continue;
                    }
                    break;
                }
                if j >= n || b[j] != b'.' {
                    push(&mut out, start, i);
                    start = i + 1;
                    last_sig = 0;
                }
            }
            c if !c.is_ascii_whitespace() => last_sig = c,
            _ => {}
        }
        i += 1;
    }
    push(&mut out, start, n);
    // Drop pieces that are only comments.
    out.retain(|(s, e)| {
        let mut p = Parser {
            s: &script[*s..*e],
            i: 0,
        };
        p.ws();
        p.i < p.s.len()
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(text: &str) -> (Option<String>, Document, Shape) {
        match parse(text).expect("parses") {
            Op::Command {
                db, command, shape, ..
            } => (db, command, shape),
            other => panic!("not a command: {other:?}"),
        }
    }

    #[test]
    fn find_with_modifiers() {
        let (db, c, shape) = cmd(
            "db.orders.find({ status: 'A', total: { $gt: 10 } }, { _id: 0 }).sort({ total: -1 }).skip(5).limit(20)",
        );
        assert_eq!(db, None);
        assert_eq!(shape, Shape::Cursor);
        assert_eq!(
            c,
            doc! {
                "find": "orders",
                "filter": { "status": "A", "total": { "$gt": 10 } },
                "projection": { "_id": 0 },
                "sort": { "total": -1 },
                "skip": 5i64,
                "limit": 20i64,
            }
        );
    }

    #[test]
    fn shell_constructors_and_regex() {
        let (_, c, _) = cmd(
            r#"db.getCollection("events").find({ _id: ObjectId("507f1f77bcf86cd799439011"), at: { $gte: ISODate("2024-05-01") }, n: NumberLong(7), name: /^ab.c/i, price: NumberDecimal("1.10"), },)"#,
        );
        let f = c.get_document("filter").expect("filter");
        assert!(matches!(f.get("_id"), Some(Bson::ObjectId(_))));
        assert_eq!(
            f.get_document("at")
                .expect("at")
                .get_datetime("$gte")
                .expect("date")
                .try_to_rfc3339_string()
                .expect("rfc3339"),
            "2024-05-01T00:00:00Z"
        );
        assert_eq!(f.get("n"), Some(&Bson::Int64(7)));
        assert_eq!(
            f.get("name"),
            Some(&Bson::RegularExpression(Regex {
                pattern: "^ab.c".into(),
                options: "i".into()
            }))
        );
        assert!(matches!(f.get("price"), Some(Bson::Decimal128(_))));
    }

    #[test]
    fn find_one_count_and_explain() {
        let (_, c, _) = cmd("db.users.findOne({ email: \"a@b.c\" })");
        assert_eq!(c.get_i64("limit"), Ok(1));
        let (_, c, shape) = cmd("db.users.find({ active: true }).count()");
        assert_eq!(shape, Shape::Count);
        assert_eq!(c, doc! { "count": "users", "query": { "active": true } });
        let (_, c, shape) = cmd("db.users.find({}).explain('executionStats')");
        assert_eq!(shape, Shape::Reply);
        assert_eq!(c.get_str("verbosity"), Ok("executionStats"));
    }

    #[test]
    fn aggregate_and_sibling_db() {
        let (db, c, shape) = cmd(
            "db.getSiblingDB('shop').orders.aggregate([{ $match: { s: 1 } }, { $group: { _id: '$c', n: { $sum: 1 } } }])",
        );
        assert_eq!(db.as_deref(), Some("shop"));
        assert_eq!(shape, Shape::Cursor);
        assert_eq!(c.get_array("pipeline").map(Vec::len), Ok(2));
        assert_eq!(parse("db.o.aggregate([{ $out: 'x' }])").map(|o| o.effect()), Ok(Effect::Write { destructive: None }));
        assert_eq!(parse("db.o.aggregate([])").map(|o| o.effect()), Ok(Effect::Read));
    }

    #[test]
    fn show_use_and_commands() {
        assert_eq!(parse("use shop;"), Ok(Op::Use("shop".into())));
        assert!(matches!(cmd("show dbs").2, Shape::Databases));
        assert_eq!(cmd("show collections").1.get_i32("listCollections"), Ok(1));
        let (db, c, _) = cmd("db.adminCommand({ ping: 1 })");
        assert_eq!(db.as_deref(), Some("admin"));
        assert_eq!(c, doc! { "ping": 1 });
        assert_eq!(cmd("{ dbStats: 1 }").1, doc! { "dbStats": 1 });
    }

    #[test]
    fn writes_and_their_effects() {
        let op = parse("db.users.insertOne({ name: 'x' })").expect("insert");
        let Op::Command {
            command, inserted, ..
        } = &op
        else {
            panic!("command")
        };
        assert_eq!(inserted.len(), 1);
        let docs = command.get_array("documents").expect("documents");
        assert_eq!(docs[0].as_document().and_then(|d| d.keys().next().cloned()).as_deref(), Some("_id"));
        assert_eq!(op.effect(), Effect::Write { destructive: None });

        let all = |s: &str| parse(s).expect("parses").effect();
        assert_eq!(
            all("db.users.deleteMany({})"),
            Effect::Write {
                destructive: Some((Destructive::DeleteAll, "users".into()))
            }
        );
        assert_eq!(all("db.users.deleteMany({ a: 1 })"), Effect::Write { destructive: None });
        assert_eq!(
            all("db.users.updateMany({}, { $set: { a: 1 } })"),
            Effect::Write {
                destructive: Some((Destructive::UpdateAll, "users".into()))
            }
        );
        assert_eq!(
            all("db.users.drop()"),
            Effect::Write {
                destructive: Some((Destructive::DropCollection, "users".into()))
            }
        );
        assert!(matches!(
            all("db.dropDatabase()"),
            Effect::Write {
                destructive: Some((Destructive::DropDatabase, _))
            }
        ));
        assert_eq!(all("db.users.countDocuments({})"), Effect::Read);
        assert_eq!(all("db.runCommand({ serverStatus: 1 })"), Effect::Read);
        assert!(parse("db.users.updateOne({}, { a: 1 })").is_err());
    }

    #[test]
    fn index_helpers() {
        let (_, c, _) = cmd("db.users.createIndex({ email: 1, at: -1 }, { unique: true })");
        assert_eq!(
            c,
            doc! { "createIndexes": "users", "indexes": [{ "key": { "email": 1, "at": -1 }, "name": "email_1_at_-1", "unique": true }] }
        );
    }

    #[test]
    fn errors_point_at_the_problem() {
        let e = parse("db.users.find({ a: })").expect_err("bad value");
        assert_eq!(e.offset, 19);
        assert!(parse("SELECT 1").is_err());
        assert!(parse("db.users.find({ a: someVar })").is_err());
        assert!(parse("db.users.frobnicate()").is_err());
    }

    #[test]
    fn splits_scripts_on_lines_and_semicolons() {
        let script = "use shop\ndb.orders.find({\n  a: 1\n})\n  .limit(5)\n\n// note\ndb.x.drop(); db.y.drop()\n";
        let parts: Vec<&str> = split(script).iter().map(|(s, e)| &script[*s..*e]).collect();
        assert_eq!(
            parts,
            [
                "use shop",
                "db.orders.find({\n  a: 1\n})\n  .limit(5)",
                "// note\ndb.x.drop()",
                "db.y.drop()"
            ]
        );
        let s2 = "db.a.find({ s: 'x;y' }) // c;\n";
        assert_eq!(split(s2).len(), 1);
    }

    #[test]
    fn relaxed_filter_documents() {
        assert_eq!(parse_document(""), Ok(Document::new()));
        assert_eq!(
            parse_document("{ 'a.b': 1, c: [1, 'x'] }"),
            Ok(doc! { "a.b": 1, "c": [1, "x"] })
        );
        assert!(parse_document("{ a: 1 } x").is_err());
    }
}
