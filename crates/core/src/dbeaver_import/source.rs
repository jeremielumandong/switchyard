//! Finding and reading DBeaver's per-project connection files: `data-sources*.json` and the
//! encrypted `credentials-config.json` beside them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use secrecy::SecretString;
use serde_json::Value;

/// The key every DBeaver install encrypts `credentials-config.json` with (AES-128-CBC; the
/// file starts with the IV). It is public; the file is obfuscated, not protected.
const CREDENTIALS_KEY: [u8; 16] = [
    0xba, 0xbb, 0x4a, 0x9f, 0x77, 0x4a, 0xb8, 0x53, 0xc9, 0x6c, 0x2d, 0x65, 0x3d, 0xfe, 0x54, 0x4a,
];

/// The DBeaver workspace folders to look in on this machine, most usual first:
/// the standard install, then Flatpak and Snap on Linux.
pub(crate) fn workspace_roots() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let mut roots = Vec::new();
    if cfg!(windows) {
        if let Some(app) = std::env::var_os("APPDATA") {
            roots.push(PathBuf::from(app).join("DBeaverData"));
        }
    } else if cfg!(target_os = "macos") {
        if let Some(h) = &home {
            roots.push(h.join("Library").join("DBeaverData"));
        }
    } else {
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".local").join("share")));
        if let Some(d) = data {
            roots.push(d.join("DBeaverData"));
        }
        if let Some(h) = &home {
            roots.push(
                h.join(".var/app/io.dbeaver.DBeaverCommunity/data")
                    .join("DBeaverData"),
            );
            roots.push(h.join("snap/dbeaver-ce/current/.local/share/DBeaverData"));
        }
    }
    roots.into_iter().map(|r| r.join("workspace6")).collect()
}

/// Every `data-sources*.json` under the projects of `workspace` (`<project>/.dbeaver/`).
pub(crate) fn data_source_files(workspace: &Path) -> Vec<PathBuf> {
    let Ok(projects) = std::fs::read_dir(workspace) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = projects
        .filter_map(|e| e.ok())
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path().join(".dbeaver"))
        .filter(|d| d.is_dir())
        .collect();
    dirs.sort();
    dirs.iter().flat_map(|d| files_in(d)).collect()
}

/// The `data-sources*.json` files in one `.dbeaver` folder, `data-sources.json` first.
fn files_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("data-sources") && n.ends_with(".json"))
        })
        .collect();
    files.sort_by_key(|p| {
        (
            p.file_name().is_some_and(|n| n != "data-sources.json"),
            p.clone(),
        )
    });
    files
}

/// The project a `<project>/.dbeaver/data-sources.json` belongs to (its folder name).
pub(crate) fn project_of(file: &Path) -> String {
    file.parent()
        .filter(|d| d.file_name().is_some_and(|n| n == ".dbeaver"))
        .and_then(|d| d.parent())
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A user and password saved for one connection or its SSH tunnel.
#[derive(Default)]
pub(crate) struct Saved {
    /// User name.
    pub user: Option<String>,
    /// Password.
    pub password: Option<SecretString>,
}

/// Saved credentials by DBeaver connection id: the database sign-in and the SSH tunnel's.
#[derive(Default)]
pub(crate) struct Credentials {
    pub db: HashMap<String, Saved>,
    pub ssh: HashMap<String, Saved>,
}

/// Decrypt a `credentials-config.json` (IV, then AES-128-CBC with PKCS#7 padding).
pub(crate) fn decrypt(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 32 || !data.len().is_multiple_of(16) {
        return None;
    }
    let (iv, body) = data.split_at(16);
    let mut buf = body.to_vec();
    let dec = cbc::Decryptor::<aes::Aes128>::new_from_slices(&CREDENTIALS_KEY, iv).ok()?;
    let n = dec.decrypt_padded_mut::<Pkcs7>(&mut buf).ok()?.len();
    buf.truncate(n);
    Some(buf)
}

/// Parse decrypted credentials:
/// `{"<id>": {"#connection": {"user", "password"}, "network/ssh_tunnel": {…}}}`.
pub(crate) fn parse_credentials(plain: &[u8]) -> Credentials {
    let mut out = Credentials::default();
    let Ok(Value::Object(all)) = serde_json::from_slice::<Value>(plain) else {
        return out;
    };
    let saved = |v: Option<&Value>| {
        v.map(|v| Saved {
            user: str_of(v, "user"),
            password: str_of(v, "password").map(SecretString::from),
        })
    };
    for (id, v) in all {
        if let Some(s) = saved(v.get("#connection")) {
            out.db.insert(id.clone(), s);
        }
        if let Some(s) = saved(v.get("network/ssh_tunnel")) {
            out.ssh.insert(id, s);
        }
    }
    out
}

/// The credentials saved beside `data_sources`, if any can be read.
pub(crate) fn read_credentials(data_sources: &Path) -> Credentials {
    let Some(dir) = data_sources.parent() else {
        return Credentials::default();
    };
    std::fs::read(dir.join("credentials-config.json"))
        .ok()
        .and_then(|d| decrypt(&d))
        .map(|p| parse_credentials(&p))
        .unwrap_or_default()
}

/// A non-empty string field (DBeaver writes ports and flags as strings or numbers).
pub(crate) fn str_of(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// A boolean field written as `true` or `"true"`.
pub(crate) fn flag(v: &Value, key: &str) -> bool {
    str_of(v, key).is_some_and(|s| s.eq_ignore_ascii_case("true"))
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use aes::cipher::BlockEncryptMut;
    use secrecy::ExposeSecret;

    /// Encrypt like DBeaver does (for fixtures).
    pub(crate) fn encrypt(plain: &[u8]) -> Vec<u8> {
        let iv = [7u8; 16];
        let enc = cbc::Encryptor::<aes::Aes128>::new_from_slices(&CREDENTIALS_KEY, &iv).unwrap();
        let mut buf = vec![0u8; plain.len() + 16];
        buf[..plain.len()].copy_from_slice(plain);
        let n = enc
            .encrypt_padded_mut::<Pkcs7>(&mut buf, plain.len())
            .unwrap()
            .len();
        let mut out = iv.to_vec();
        out.extend_from_slice(&buf[..n]);
        out
    }

    #[test]
    fn decrypts_dbeaver_credentials() {
        // Made with `openssl enc -aes-128-cbc -K babb4a9f774ab853c96c2d653dfe544a
        // -iv 000102030405060708090a0b0c0d0e0f`, IV prepended, as DBeaver writes it.
        let file: Vec<u8> = ["000102030405060708090a0b0c0d0e0f", OPENSSL_CIPHERTEXT]
            .concat()
            .as_bytes()
            .chunks(2)
            .map(|h| u8::from_str_radix(std::str::from_utf8(h).unwrap(), 16).unwrap())
            .collect();
        let creds = parse_credentials(&decrypt(&file).unwrap());
        let db = &creds.db["postgres-jdbc-1"];
        assert_eq!(db.user.as_deref(), Some("app"));
        assert_eq!(db.password.as_ref().unwrap().expose_secret(), "s3cret");
        assert_eq!(creds.ssh["postgres-jdbc-1"].user.as_deref(), Some("ops"));
    }

    const OPENSSL_CIPHERTEXT: &str = include_str!("testdata/credentials.hex");

    #[test]
    fn round_trips_and_rejects_garbage() {
        let plain = br##"{"a":{"#connection":{"user":"u"}}}"##;
        assert_eq!(decrypt(&encrypt(plain)).unwrap(), plain);
        assert!(decrypt(b"short").is_none());
        assert!(decrypt(&[0u8; 48]).is_none(), "bad padding");
    }

    #[test]
    fn finds_projects_and_their_files() {
        let ws = tempfile::tempdir().unwrap();
        for (project, file) in [
            ("General", "data-sources.json"),
            ("General", "data-sources-2.json"),
            ("Work", "data-sources.json"),
            (".metadata", "data-sources.json"),
        ] {
            let d = ws.path().join(project).join(".dbeaver");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(file), "{}").unwrap();
        }
        let files = data_source_files(ws.path());
        let shown: Vec<(String, String)> = files
            .iter()
            .map(|f| {
                (
                    project_of(f),
                    f.file_name().unwrap().to_string_lossy().into_owned(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                ("General".into(), "data-sources.json".into()),
                ("General".into(), "data-sources-2.json".into()),
                ("Work".into(), "data-sources.json".into()),
            ]
        );
    }
}
