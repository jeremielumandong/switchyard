//! Terminal macros (MX-5): keystrokes recorded in a terminal, saved with a name and
//! replayed into a terminal (or every broadcast pane).
//!
//! The recorded bytes are kept as an escaped string (`\r`, `\e[A`, `\x01`, …) so a saved
//! macro stays readable and editable in an export.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::model::ValidationError;

/// Longest macro, in bytes of input.
pub const MAX_MACRO_BYTES: usize = 64 * 1024;

/// A saved terminal macro.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Macro {
    /// Id; empty before it is stored.
    pub id: String,
    /// Display name.
    pub name: String,
    /// The bytes typed, as sent to the program.
    #[serde(serialize_with = "ser_input", deserialize_with = "de_input")]
    pub input: Vec<u8>,
    /// Creation time, ms since epoch.
    #[serde(default)]
    pub created_at: i64,
    /// Last change, ms since epoch.
    #[serde(default)]
    pub updated_at: i64,
}

impl Macro {
    /// A new, unsaved macro.
    pub fn new(name: &str, input: Vec<u8>) -> Self {
        Self {
            id: String::new(),
            name: name.to_owned(),
            input,
            created_at: 0,
            updated_at: 0,
        }
    }

    /// Check the fields before storing.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let invalid = |field: &'static str, message: &str| ValidationError {
            field,
            message: message.to_owned(),
        };
        if self.name.trim().is_empty() {
            return Err(invalid("name", "a macro needs a name"));
        }
        if self.input.is_empty() {
            return Err(invalid("input", "the macro is empty"));
        }
        if self.input.len() > MAX_MACRO_BYTES {
            return Err(invalid("input", "the macro is too long (64 KB at most)"));
        }
        Ok(())
    }

    /// The input as an escaped string (see [`escape_input`]).
    pub fn escaped(&self) -> String {
        escape_input(&self.input)
    }
}

/// Bytes as a readable string: text as is, `\\`, `\r`, `\n`, `\t`, `\e` (ESC) and `\xHH`
/// for other control characters and bytes that are not UTF-8.
pub fn escape_input(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\r' => out.push_str("\\r"),
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                '\x1b' => out.push_str("\\e"),
                c if c.is_ascii_control() => {
                    out.push_str(&format!("\\x{:02x}", c as u32));
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

/// The bytes of an [`escape_input`] string. An unknown or cut-off escape is kept as text.
pub fn unescape_input(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.peek().map(|&(_, n)| n) {
            Some('\\') => out.push(b'\\'),
            Some('r') => out.push(b'\r'),
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('e') => out.push(0x1b),
            Some('x') => {
                match s
                    .get(i + 2..i + 4)
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(b) => {
                        out.push(b);
                        chars.next();
                        chars.next();
                    }
                    None => {
                        out.push(b'\\');
                        continue;
                    }
                }
            }
            _ => {
                out.push(b'\\');
                continue;
            }
        }
        chars.next();
    }
    out
}

fn ser_input<S: Serializer>(input: &[u8], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&escape_input(input))
}

fn de_input<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let s = String::deserialize(d)?;
    Ok(unescape_input(&s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_round_trips_every_byte() {
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(unescape_input(&escape_input(&all)), all);
        let typed = "ls -la ~/dir\\ x\r\x1b[A\x03héllo\t".as_bytes();
        let escaped = escape_input(typed);
        assert_eq!(escaped, "ls -la ~/dir\\\\ x\\r\\e[A\\x03héllo\\t");
        assert_eq!(unescape_input(&escaped), typed);
    }

    #[test]
    fn unknown_escapes_stay_as_text() {
        assert_eq!(unescape_input("a\\qb\\x4"), b"a\\qb\\x4");
        assert_eq!(unescape_input("\\x41\\"), b"A\\");
    }

    #[test]
    fn serializes_input_as_a_readable_string() {
        let m = Macro::new("deploy", b"cd /srv\rmake\r".to_vec());
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains(r#""input":"cd /srv\\rmake\\r""#), "{json}");
        let back: Macro = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn validation() {
        assert!(Macro::new(" ", b"x".to_vec()).validate().is_err());
        assert!(Macro::new("a", Vec::new()).validate().is_err());
        assert!(
            Macro::new("a", vec![b'x'; MAX_MACRO_BYTES + 1])
                .validate()
                .is_err()
        );
        assert!(Macro::new("a", b"x".to_vec()).validate().is_ok());
    }
}
