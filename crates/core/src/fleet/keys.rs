//! SSH key rollout: pure editing of `~/.ssh/authorized_keys` text (add idempotently,
//! remove every line holding a key, refuse to remove the key the session signed in with)
//! and the shell commands that read and replace the file over SSH.
//!
//! Keys are compared by their base64 blob, so options (`from="…"`, `command="…"`) and
//! comments never hide a match. Lines that are not keys (comments, blank lines, keys
//! this build cannot read) are kept exactly as they were.

use serde::{Deserialize, Serialize};

/// Backup written before every change, next to the file.
pub const BACKUP_NAME: &str = "authorized_keys.switchyard-bak";

/// Exit status of [`READ_COMMAND`] when the file does not exist.
pub const MISSING_STATUS: u32 = 3;

/// Print the file; exit [`MISSING_STATUS`] when there is none.
pub const READ_COMMAND: &str =
    "f=\"$HOME/.ssh/authorized_keys\"; [ -f \"$f\" ] || exit 3; cat -- \"$f\"";

/// Replace the file with standard input: create `~/.ssh` (0700) when missing, copy the
/// old file to [`BACKUP_NAME`], write the new one beside it (0600) and rename it into
/// place, so sshd never sees a half-written file. SELinux labels are restored when the
/// tool exists.
pub const WRITE_COMMAND: &str = "set -e; umask 077; d=\"$HOME/.ssh\"; \
mkdir -p -- \"$d\"; chmod 700 -- \"$d\" 2>/dev/null || true; cd -- \"$d\"; \
if [ -f authorized_keys ]; then cp -p -- authorized_keys authorized_keys.switchyard-bak; fi; \
cat > authorized_keys.switchyard-new; chmod 600 authorized_keys.switchyard-new; \
mv -f -- authorized_keys.switchyard-new authorized_keys; \
if command -v restorecon >/dev/null 2>&1; then restorecon authorized_keys 2>/dev/null || true; fi";

/// Why a key could not be used or removed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyEditError {
    /// The text is not an OpenSSH public key line.
    #[error("not an OpenSSH public key (expected `ssh-ed25519 AAAA… comment`)")]
    NotAKey,
    /// The key is the one this session signed in with; removing it could lock you out.
    #[error("this is the key Switchyard signed in with; it is kept so you are not locked out")]
    InUse,
}

/// One public key: its type, base64 blob and comment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicKeyLine {
    /// `ssh-ed25519`, `ecdsa-sha2-nistp256`, `ssh-rsa`, `sk-…`.
    pub kind: String,
    /// Base64 key blob.
    pub blob: String,
    /// Comment (often `user@host`).
    pub comment: String,
}

impl PublicKeyLine {
    /// The line as `authorized_keys` holds it.
    pub fn to_line(&self) -> String {
        if self.comment.is_empty() {
            format!("{} {}", self.kind, self.blob)
        } else {
            format!("{} {} {}", self.kind, self.blob, self.comment)
        }
    }

    /// `SHA256:…`, when this build can read the key.
    pub fn fingerprint(&self) -> Option<String> {
        switchyard_remote::ssh::blob_fingerprint(&self.blob)
    }
}

/// A key found in an `authorized_keys` file (audit).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizedKey {
    /// 1-based line number.
    pub line: usize,
    /// Options before the key (`from="10.0.0.0/8",no-pty`), if any.
    pub options: Option<String>,
    /// The key.
    pub key: PublicKeyLine,
    /// `SHA256:…`, or `None` for a key type this build cannot read.
    pub fingerprint: Option<String>,
}

fn is_key_type(word: &str) -> bool {
    word.starts_with("ssh-")
        || word.starts_with("ecdsa-sha2-")
        || word.starts_with("sk-ssh-")
        || word.starts_with("sk-ecdsa-")
}

/// Split the options field off a line: options may hold quoted spaces.
fn split_options(line: &str) -> (Option<&str>, &str) {
    let first_word = line.split_whitespace().next().unwrap_or("");
    if is_key_type(first_word) {
        return (None, line);
    }
    let mut quoted = false;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => return (Some(&line[..i]), line[i..].trim_start()),
            _ => {}
        }
    }
    (Some(line), "")
}

/// A key line without options (`kind blob comment…`).
fn key_part(rest: &str) -> Option<PublicKeyLine> {
    let mut words = rest.split_whitespace();
    let kind = words.next()?;
    let blob = words.next()?;
    if !is_key_type(kind) || blob.is_empty() {
        return None;
    }
    Some(PublicKeyLine {
        kind: kind.to_owned(),
        blob: blob.to_owned(),
        comment: words.collect::<Vec<_>>().join(" "),
    })
}

/// Parse a public key as pasted from a `.pub` file (one line, no options).
pub fn parse_public_key(text: &str) -> Result<PublicKeyLine, KeyEditError> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .ok_or(KeyEditError::NotAKey)?;
    let key = key_part(line).ok_or(KeyEditError::NotAKey)?;
    if key.fingerprint().is_none() {
        return Err(KeyEditError::NotAKey);
    }
    Ok(key)
}

/// The key on one `authorized_keys` line, with its options.
fn parse_line(line: &str) -> Option<(Option<String>, PublicKeyLine)> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    let (options, rest) = split_options(t);
    key_part(rest).map(|k| (options.map(str::to_owned), k))
}

/// Every key in the file, in order.
pub fn parse_authorized_keys(text: &str) -> Vec<AuthorizedKey> {
    text.lines()
        .enumerate()
        .filter_map(|(i, l)| {
            let (options, key) = parse_line(l)?;
            Some(AuthorizedKey {
                line: i + 1,
                options,
                fingerprint: key.fingerprint(),
                key,
            })
        })
        .collect()
}

/// Whether the file holds `key`.
pub fn has_key(text: &str, key: &PublicKeyLine) -> bool {
    text.lines()
        .filter_map(parse_line)
        .any(|(_, k)| k.blob == key.blob)
}

/// The file with `key` appended, or `None` when it is already there (idempotent).
pub fn add_key(text: &str, key: &PublicKeyLine) -> Option<String> {
    if has_key(text, key) {
        return None;
    }
    let mut out = text.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&key.to_line());
    out.push('\n');
    Some(out)
}

/// The file without any line holding `key`, and how many lines went; `None` when it is
/// not there. Refused when `key` is the session's own (`session_fingerprint`).
pub fn remove_key(
    text: &str,
    key: &PublicKeyLine,
    session_fingerprint: Option<&str>,
) -> Result<Option<(String, usize)>, KeyEditError> {
    if let (Some(own), Some(fp)) = (session_fingerprint, key.fingerprint())
        && own == fp
    {
        return Err(KeyEditError::InUse);
    }
    let mut removed = 0;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        if parse_line(line).is_some_and(|(_, k)| k.blob == key.blob) {
            removed += 1;
        } else {
            out.push_str(line);
        }
    }
    Ok((removed > 0).then_some((out, removed)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const ED_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIJbqpzfWlUVlVlDiqty/YbNbN/FB08djdOD5Fh00QTf0";
    const ED_B: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIAYUhAj9rEnudntRTasvH4O9GRXi5pQ4MXUj8OdSVQlO";

    fn key(blob: &str, comment: &str) -> PublicKeyLine {
        parse_public_key(&format!("ssh-ed25519 {blob} {comment}")).unwrap()
    }

    fn file() -> String {
        format!(
            "# managed by hand\n\
             ssh-ed25519 {ED_A} alice@laptop\n\
             \n\
             from=\"10.0.0.0/8\",command=\"echo hi there\" ssh-ed25519 {ED_B} deploy@ci\n\
             ssh-unknown-type AAAAnotreal weird\n"
        )
    }

    #[test]
    fn parses_keys_with_options_and_skips_the_rest() {
        let keys = parse_authorized_keys(&file());
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0].line, 2);
        assert_eq!(keys[0].key.comment, "alice@laptop");
        assert!(
            keys[0]
                .fingerprint
                .as_deref()
                .unwrap()
                .starts_with("SHA256:")
        );
        assert_eq!(
            keys[1].options.as_deref(),
            Some("from=\"10.0.0.0/8\",command=\"echo hi there\"")
        );
        assert_eq!(keys[1].key.blob, ED_B);
        // A type this build cannot read is listed without a fingerprint.
        assert_eq!(keys[2].fingerprint, None);
        assert!(parse_public_key("hello world").is_err());
        assert!(parse_public_key("ssh-ed25519 AAAAbroken x").is_err());
        assert!(parse_public_key(&format!("\n# c\nssh-ed25519 {ED_A}\n")).is_ok());
    }

    #[test]
    fn add_is_idempotent_and_keeps_the_file() {
        let a = key(ED_A, "other comment");
        // Present (different comment): nothing to do.
        assert_eq!(add_key(&file(), &a), None);
        let only_b = format!("ssh-ed25519 {ED_B} deploy@ci");
        let added = add_key(&only_b, &a).unwrap();
        assert_eq!(
            added,
            format!("ssh-ed25519 {ED_B} deploy@ci\nssh-ed25519 {ED_A} other comment\n")
        );
        assert_eq!(add_key(&added, &a), None);
        assert_eq!(add_key("", &a).unwrap(), format!("{}\n", a.to_line()));
    }

    #[test]
    fn remove_drops_every_line_of_the_key() {
        let b = key(ED_B, "");
        let text = format!("{}ssh-ed25519 {ED_B} again\n", file());
        let (out, n) = remove_key(&text, &b, None).unwrap().unwrap();
        assert_eq!(n, 2);
        assert!(!has_key(&out, &b));
        assert_eq!(
            out,
            format!(
                "# managed by hand\nssh-ed25519 {ED_A} alice@laptop\n\nssh-unknown-type AAAAnotreal weird\n"
            )
        );
        // Absent: nothing to do.
        assert_eq!(remove_key(&out, &b, None).unwrap(), None);
    }

    #[test]
    fn the_session_key_is_never_removed() {
        let a = key(ED_A, "");
        let own = a.fingerprint().unwrap();
        assert_eq!(
            remove_key(&file(), &a, Some(&own)),
            Err(KeyEditError::InUse)
        );
        // Another key goes even while signed in with this one.
        let b = key(ED_B, "");
        assert!(remove_key(&file(), &b, Some(&own)).unwrap().is_some());
    }
}
