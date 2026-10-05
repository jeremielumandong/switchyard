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

    fn entry(key: &SecretRef) -> Result<keyring::Entry> {
        keyring::Entry::new(SERVICE, &key.0).map_err(|e| StoreError::Keychain(e.to_string()))
    }
}

impl SecretStore for KeychainStore {
    fn backend(&self) -> &'static str {
        "OS keychain"
    }

    fn get(&self, key: &SecretRef) -> Result<Option<SecretString>> {
        debug!(key = %key.0, "keychain read");
        match Self::entry(key)?.get_password() {
            Ok(v) => Ok(Some(SecretString::from(v))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(StoreError::Keychain(e.to_string())),
        }
    }

    fn set(&self, key: &SecretRef, value: &SecretString) -> Result<()> {
        debug!(key = %key.0, "keychain write");
        Self::entry(key)?
            .set_password(value.expose_secret())
            .map_err(|e| StoreError::Keychain(e.to_string()))
    }

    fn delete(&self, key: &SecretRef) -> Result<()> {
        debug!(key = %key.0, "keychain delete");
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(StoreError::Keychain(e.to_string())),
        }
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

    #[test]
    #[ignore = "needs an OS keychain / Secret Service"]
    fn keychain_round_trip() {
        assert!(KeychainStore::available());
        let logs = with_captured_logs(|| exercise(&KeychainStore));
        assert!(!logs.contains(SECRET));
    }
}
