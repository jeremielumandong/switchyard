//! Secret storage: the OS keychain via `keyring`, or an encrypted local vault
//! (argon2id + ChaCha20-Poly1305) unlocked by a master password when no keychain exists.
//!
//! Secret values are `SecretString`s; they are never logged, serialized into profiles or
//! included in errors. Only key names appear in logs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use argon2::Argon2;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::error::{Result, StoreError};
use crate::model::SecretRef;
use crate::random::{hex, random_bytes, unhex};

/// Keychain service name.
pub const SERVICE: &str = "dev.switchyard.app";

/// A place secrets live.
pub trait SecretStore: Send + Sync {
    /// Human-readable backend name.
    fn backend(&self) -> &'static str;
    /// Read a secret.
    fn get(&self, key: &SecretRef) -> Result<Option<SecretString>>;
    /// Write a secret.
    fn set(&self, key: &SecretRef, value: &SecretString) -> Result<()>;
    /// Delete a secret (missing is fine).
    fn delete(&self, key: &SecretRef) -> Result<()>;
}

/// OS keychain backend.
#[derive(Debug, Default)]
pub struct KeychainStore;

impl KeychainStore {
    /// Whether the platform keychain is usable (Linux needs a Secret Service).
    pub fn available() -> bool {
        keyring::Entry::store_status().is_ok()
    }
}

/// One keychain item, by name.
fn entry(name: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, name).map_err(|e| StoreError::Keychain(e.to_string()))
}

/// The OS keychain as plain named strings, for [`chunked`].
struct OsItems;

impl chunked::Items for OsItems {
    fn get(&self, name: &str) -> Result<Option<String>> {
        match entry(name)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(StoreError::Keychain(e.to_string())),
        }
    }

    fn set(&self, name: &str, value: &str) -> Result<()> {
        entry(name)?
            .set_password(value)
            .map_err(|e| StoreError::Keychain(e.to_string()))
    }

    fn delete(&self, name: &str) -> Result<()> {
        match entry(name)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(StoreError::Keychain(e.to_string())),
        }
    }
}

impl SecretStore for KeychainStore {
    fn backend(&self) -> &'static str {
        "OS keychain"
    }

    fn get(&self, key: &SecretRef) -> Result<Option<SecretString>> {
        debug!(key = %key.0, "keychain read");
        Ok(chunked::get(&OsItems, &key.0)?.map(SecretString::from))
    }

    fn set(&self, key: &SecretRef, value: &SecretString) -> Result<()> {
        debug!(key = %key.0, "keychain write");
        chunked::set(&OsItems, &key.0, value.expose_secret())
    }

    fn delete(&self, key: &SecretRef) -> Result<()> {
        debug!(key = %key.0, "keychain delete");
        chunked::delete(&OsItems, &key.0)
    }
}

/// Values longer than one keychain item holds (Windows Credential Manager: 2,560 bytes,
/// i.e. 1,280 UTF-16 units; an Entra token cache is longer) are split over numbered items
/// `<key>#1`, `<key>#2`, …; the item `<key>` then holds a marker with the part count.
/// Short values are stored as they always were.
mod chunked {
    use super::{Result, StoreError};

    /// Most UTF-16 units per item, under Windows' 1,280 with room to spare.
    pub(super) const PART_UNITS: usize = 1_000;
    /// Starts the marker item. A value that itself starts with it is stored in parts too.
    const MARKER: &str = "switchyard-parts:v1:";

    /// Named string items (the keychain, or a map in tests).
    pub(super) trait Items {
        fn get(&self, name: &str) -> Result<Option<String>>;
        fn set(&self, name: &str, value: &str) -> Result<()>;
        fn delete(&self, name: &str) -> Result<()>;
    }

    fn part(key: &str, i: usize) -> String {
        format!("{key}#{i}")
    }

    /// The part count when `value` is a marker.
    fn parts_of(value: &str) -> Option<usize> {
        value.strip_prefix(MARKER)?.parse().ok()
    }

    /// `value` cut at char boundaries into pieces of at most [`PART_UNITS`] UTF-16 units.
    pub(super) fn split(value: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let (mut start, mut units) = (0, 0);
        for (i, c) in value.char_indices() {
            if units + c.len_utf16() > PART_UNITS {
                out.push(&value[start..i]);
                (start, units) = (i, 0);
            }
            units += c.len_utf16();
        }
        out.push(&value[start..]);
        out
    }

    pub(super) fn get(items: &dyn Items, key: &str) -> Result<Option<String>> {
        let Some(head) = items.get(key)? else {
            return Ok(None);
        };
        let Some(n) = parts_of(&head) else {
            return Ok(Some(head));
        };
        let mut value = String::new();
        for i in 1..=n {
            match items.get(&part(key, i))? {
                Some(p) => value.push_str(&p),
                None => {
                    return Err(StoreError::Keychain(format!(
                        "part {i} of {n} of a long secret is missing; save it again"
                    )));
                }
            }
        }
        Ok(Some(value))
    }

    pub(super) fn set(items: &dyn Items, key: &str, value: &str) -> Result<()> {
        let old = items.get(key).ok().flatten().and_then(|h| parts_of(&h));
        let pieces = split(value);
        let new = if pieces.len() > 1 || value.starts_with(MARKER) {
            for (i, p) in pieces.iter().enumerate() {
                items.set(&part(key, i + 1), p)?;
            }
            items.set(key, &format!("{MARKER}{}", pieces.len()))?;
            pieces.len()
        } else {
            items.set(key, value)?;
            0
        };
        for i in new + 1..=old.unwrap_or(0) {
            items.delete(&part(key, i))?;
        }
        Ok(())
    }

    pub(super) fn delete(items: &dyn Items, key: &str) -> Result<()> {
        if let Some(n) = items.get(key).ok().flatten().and_then(|h| parts_of(&h)) {
            for i in 1..=n {
                items.delete(&part(key, i))?;
            }
        }
        items.delete(key)
    }
}

/// In-memory backend for tests and ephemeral sessions.
#[derive(Default)]
pub struct MemoryStore {
    map: Mutex<BTreeMap<String, SecretString>>,
}

impl SecretStore for MemoryStore {
    fn backend(&self) -> &'static str {
        "memory"
    }

    fn get(&self, key: &SecretRef) -> Result<Option<SecretString>> {
        debug!(key = %key.0, "memory read");
        Ok(self
            .map
            .lock()
            .map_err(|_| StoreError::VaultLocked)?
            .get(&key.0)
            .cloned())
    }

    fn set(&self, key: &SecretRef, value: &SecretString) -> Result<()> {
        debug!(key = %key.0, "memory write");
        self.map
            .lock()
            .map_err(|_| StoreError::VaultLocked)?
            .insert(key.0.clone(), value.clone());
        Ok(())
    }

    fn delete(&self, key: &SecretRef) -> Result<()> {
        self.map
            .lock()
            .map_err(|_| StoreError::VaultLocked)?
            .remove(&key.0);
        Ok(())
    }
}

/// On-disk vault format. Everything but the salt and parameters is ciphertext.
#[derive(Serialize, Deserialize)]
struct VaultFile {
    version: u32,
    /// Argon2id salt (hex).
    salt: String,
    /// Encrypted check value proving the key is right.
    check: Sealed,
    /// Encrypted entries keyed by secret ref.
    entries: BTreeMap<String, Sealed>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Sealed {
    nonce: String,
    data: String,
}

const CHECK_PLAINTEXT: &[u8] = b"switchyard-vault-v1";

/// Encrypted local vault for systems without a keychain.
pub struct VaultStore {
    path: PathBuf,
    state: Mutex<Option<(ChaCha20Poly1305, VaultFile)>>,
}

fn derive_key(password: &SecretString, salt: &[u8]) -> Result<ChaCha20Poly1305> {
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.expose_secret().as_bytes(), salt, &mut key)
        .map_err(|_| StoreError::BadPassword)?;
    let cipher = ChaCha20Poly1305::new(&Key::from(key));
    key.fill(0);
    Ok(cipher)
}

fn seal(cipher: &ChaCha20Poly1305, plaintext: &[u8]) -> Result<Sealed> {
    let nonce_bytes: [u8; 12] = random_bytes();
    let nonce = Nonce::from(nonce_bytes);
    let data = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|_| StoreError::BadPassword)?;
    Ok(Sealed {
        nonce: hex(&nonce_bytes),
        data: hex(&data),
    })
}

fn open(cipher: &ChaCha20Poly1305, sealed: &Sealed) -> Result<Vec<u8>> {
    let nonce: [u8; 12] = unhex(&sealed.nonce)
        .and_then(|n| n.try_into().ok())
        .ok_or(StoreError::BadPassword)?;
    let data = unhex(&sealed.data).ok_or(StoreError::BadPassword)?;
    cipher
        .decrypt(&Nonce::from(nonce), data.as_slice())
        .map_err(|_| StoreError::BadPassword)
}

impl VaultStore {
    /// A vault at `path`, initially locked.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            state: Mutex::new(None),
        }
    }

    /// Whether a vault file exists yet.
    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Whether the vault is unlocked.
    pub fn is_unlocked(&self) -> bool {
        self.state.lock().map(|s| s.is_some()).unwrap_or(false)
    }

    /// Unlock with the master password, creating the vault if it does not exist.
    pub fn unlock(&self, password: &SecretString) -> Result<()> {
        let file = if self.path.exists() {
            let raw = std::fs::read_to_string(&self.path)?;
            let file: VaultFile = serde_json::from_str(&raw)?;
            let salt = unhex(&file.salt).ok_or(StoreError::BadPassword)?;
            let cipher = derive_key(password, &salt)?;
            if open(&cipher, &file.check)? != CHECK_PLAINTEXT {
                return Err(StoreError::BadPassword);
            }
            (cipher, file)
        } else {
            let salt: [u8; 16] = random_bytes();
            let cipher = derive_key(password, &salt)?;
            let check = seal(&cipher, CHECK_PLAINTEXT)?;
            let file = VaultFile {
                version: 1,
                salt: hex(&salt),
                check,
                entries: BTreeMap::new(),
            };
            write_atomic(&self.path, &serde_json::to_vec_pretty(&file)?)?;
            (cipher, file)
        };
        info!("vault unlocked");
        *self.state.lock().map_err(|_| StoreError::VaultLocked)? = Some(file);
        Ok(())
    }

    /// Forget the key.
    pub fn lock(&self) {
        if let Ok(mut s) = self.state.lock() {
            *s = None;
        }
    }

    fn save(&self, file: &VaultFile) -> Result<()> {
        write_atomic(&self.path, &serde_json::to_vec_pretty(file)?)
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

impl SecretStore for VaultStore {
    fn backend(&self) -> &'static str {
        "encrypted vault"
    }

    fn get(&self, key: &SecretRef) -> Result<Option<SecretString>> {
        debug!(key = %key.0, "vault read");
        let guard = self.state.lock().map_err(|_| StoreError::VaultLocked)?;
        let (cipher, file) = guard.as_ref().ok_or(StoreError::VaultLocked)?;
        match file.entries.get(&key.0) {
            None => Ok(None),
            Some(sealed) => {
                let bytes = open(cipher, sealed)?;
                let s = String::from_utf8(bytes).map_err(|_| StoreError::BadPassword)?;
                Ok(Some(SecretString::from(s)))
            }
        }
    }

    fn set(&self, key: &SecretRef, value: &SecretString) -> Result<()> {
        debug!(key = %key.0, "vault write");
        let mut guard = self.state.lock().map_err(|_| StoreError::VaultLocked)?;
        let (cipher, file) = guard.as_mut().ok_or(StoreError::VaultLocked)?;
        let sealed = seal(cipher, value.expose_secret().as_bytes())?;
        file.entries.insert(key.0.clone(), sealed);
        self.save(file)
    }

    fn delete(&self, key: &SecretRef) -> Result<()> {
        debug!(key = %key.0, "vault delete");
        let mut guard = self.state.lock().map_err(|_| StoreError::VaultLocked)?;
        let (_, file) = guard.as_mut().ok_or(StoreError::VaultLocked)?;
        if file.entries.remove(&key.0).is_some() {
            self.save(file)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn with_captured_logs(f: impl FnOnce()) -> String {
        let cap = Capture::default();
        let writer = cap.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8(cap.0.lock().unwrap().clone()).unwrap()
    }

    const SECRET: &str = "hunter2-very-secret";

    fn exercise(store: &dyn SecretStore) {
        let key = SecretRef("abc:password".into());
        assert!(store.get(&key).unwrap().is_none());
        store.set(&key, &SecretString::from(SECRET)).unwrap();
        assert_eq!(store.get(&key).unwrap().unwrap().expose_secret(), SECRET);
        store.delete(&key).unwrap();
        assert!(store.get(&key).unwrap().is_none());
        // Debug output of a SecretString is redacted too.
        let dbg = format!("{:?}", SecretString::from(SECRET));
        assert!(!dbg.contains(SECRET));
    }

    #[test]
    fn memory_backend_never_logs_secret() {
        let logs = with_captured_logs(|| exercise(&MemoryStore::default()));
        assert!(
            logs.contains("abc:password"),
            "key names are logged: {logs}"
        );
        assert!(!logs.contains(SECRET));
    }

    #[test]
    fn vault_round_trip_and_wrong_password() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let logs = with_captured_logs(|| {
            let vault = VaultStore::new(&path);
            assert!(matches!(
                vault.get(&SecretRef("x".into())),
                Err(StoreError::VaultLocked)
            ));
            vault.unlock(&SecretString::from("master pw")).unwrap();
            exercise(&vault);
            vault
                .set(&SecretRef("keep".into()), &SecretString::from(SECRET))
                .unwrap();
        });
        assert!(!logs.contains(SECRET));
        assert!(!logs.contains("master pw"));
        // The file holds ciphertext only.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains(SECRET));
        // Re-open: right password reads it, wrong one is refused.
        let vault = VaultStore::new(&path);
        assert!(matches!(
            vault.unlock(&SecretString::from("wrong")),
            Err(StoreError::BadPassword)
        ));
        vault.unlock(&SecretString::from("master pw")).unwrap();
        let got = vault.get(&SecretRef("keep".into())).unwrap().unwrap();
        assert_eq!(got.expose_secret(), SECRET);
        vault.lock();
        assert!(!vault.is_unlocked());
    }

    #[derive(Default)]
    struct MapItems(Mutex<BTreeMap<String, String>>);

    impl chunked::Items for MapItems {
        fn get(&self, name: &str) -> Result<Option<String>> {
            Ok(self.0.lock().unwrap().get(name).cloned())
        }
        fn set(&self, name: &str, value: &str) -> Result<()> {
            // Windows Credential Manager's limit, in UTF-16 units.
            if value.encode_utf16().count() > 1_280 {
                return Err(StoreError::Keychain("longer than platform limit".into()));
            }
            self.0.lock().unwrap().insert(name.into(), value.into());
            Ok(())
        }
        fn delete(&self, name: &str) -> Result<()> {
            self.0.lock().unwrap().remove(name);
            Ok(())
        }
    }

    #[test]
    fn long_secrets_are_split_over_items() {
        let items = MapItems::default();
        // An Entra token cache is a few KB of JSON; mix in non-BMP chars (2 UTF-16 units).
        let long = format!("{{\"refresh\":\"{}\"}}", "ab😀".repeat(2_000));
        chunked::set(&items, "c:token", &long).unwrap();
        assert_eq!(chunked::get(&items, "c:token").unwrap().unwrap(), long);
        let n = items.0.lock().unwrap().len();
        assert!(n > 2, "split into parts: {n} items");

        // Shorter again: one plain item, stale parts removed.
        chunked::set(&items, "c:token", "short").unwrap();
        assert_eq!(chunked::get(&items, "c:token").unwrap().unwrap(), "short");
        assert_eq!(items.0.lock().unwrap().len(), 1);

        chunked::set(&items, "c:token", &long).unwrap();
        chunked::delete(&items, "c:token").unwrap();
        assert!(items.0.lock().unwrap().is_empty());
        assert!(chunked::get(&items, "c:token").unwrap().is_none());

        // Every piece fits and they rebuild the value.
        let pieces = chunked::split(&long);
        assert!(
            pieces
                .iter()
                .all(|p| p.encode_utf16().count() <= chunked::PART_UNITS)
        );
        assert_eq!(pieces.concat(), long);
    }

    #[test]
    fn missing_part_is_an_error() {
        let items = MapItems::default();
        chunked::set(&items, "k", &"x".repeat(5_000)).unwrap();
        items.0.lock().unwrap().remove("k#2");
        assert!(chunked::get(&items, "k").is_err());
    }

    #[test]
    #[ignore = "needs an OS keychain / Secret Service"]
    fn keychain_round_trip() {
        assert!(KeychainStore::available());
        let logs = with_captured_logs(|| exercise(&KeychainStore));
        assert!(!logs.contains(SECRET));
    }
}
