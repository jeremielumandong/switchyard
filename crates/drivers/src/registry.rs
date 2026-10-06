//! The component registry: the active manifest, user-chosen paths (persisted in
//! `<drivers_dir>/paths.json`), detection, removal, and runtime loading with `libloading`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use libloading::Library;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::detect::{self, ComponentStatus, DetectEnv, Source};
use crate::error::{DriverError, Result};
use crate::install::{self, InstallPlan};
use crate::manifest::{ComponentSpec, License, Manifest};

/// A component as shown on the card and Settings → Drivers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Component {
    /// Manifest id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// The feature that needs it.
    pub needed_for: String,
    /// Detected status.
    pub status: ComponentStatus,
    /// What "Install automatically" would do on this machine.
    pub plan: InstallPlan,
    /// License, if the manifest names one.
    pub license: Option<License>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The Driver Manager's state.
pub struct Registry {
    manifest: Mutex<Manifest>,
    env: Mutex<DetectEnv>,
    mirror: Mutex<Option<String>>,
    has: Box<dyn Fn(&str) -> bool + Send + Sync>,
    loaded: Mutex<HashMap<String, Arc<Library>>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("env", &*lock(&self.env))
            .finish_non_exhaustive()
    }
}

impl Registry {
    /// The real machine, with the bundled manifest.
    pub fn new(drivers_dir: PathBuf) -> Self {
        Self::with_env(
            Manifest::bundled(),
            DetectEnv::system(drivers_dir),
            Box::new(install::on_path),
        )
    }

    /// A registry over any manifest and environment (tests).
    pub fn with_env(
        manifest: Manifest,
        mut env: DetectEnv,
        has: Box<dyn Fn(&str) -> bool + Send + Sync>,
    ) -> Self {
        env.overrides = read_overrides(&env.drivers_dir);
        Self {
            manifest: Mutex::new(manifest),
            env: Mutex::new(env),
            mirror: Mutex::new(None),
            has,
            loaded: Mutex::default(),
        }
    }

    /// App-managed driver directory.
    pub fn drivers_dir(&self) -> PathBuf {
        lock(&self.env).drivers_dir.clone()
    }

    /// Replace the manifest (a newer, verified one from the update server).
    pub fn set_manifest(&self, m: Manifest) {
        *lock(&self.manifest) = m;
    }

    /// Download archives from this base URL instead of the vendor's.
    pub fn set_mirror(&self, mirror: Option<String>) {
        *lock(&self.mirror) = mirror.filter(|m| !m.trim().is_empty());
    }

    /// A component's manifest entry.
    pub fn spec(&self, id: &str) -> Result<ComponentSpec> {
        lock(&self.manifest).component(id).cloned()
    }

    fn describe(&self, spec: &ComponentSpec, env: &DetectEnv) -> Component {
        let mirror = lock(&self.mirror).clone();
        Component {
            id: spec.id.clone(),
            name: spec.name.clone(),
            needed_for: spec.needed_for.clone(),
            status: detect::detect(spec, env),
            plan: install::plan(spec, env.os, mirror.as_deref(), &*self.has),
            license: spec.license.clone(),
        }
    }

    /// Detect every component (file system access: call off the UI thread).
    pub fn components(&self) -> Vec<Component> {
        let specs = lock(&self.manifest).components.clone();
        let env = lock(&self.env);
        specs.iter().map(|s| self.describe(s, &env)).collect()
    }

    /// Detect one component.
    pub fn component(&self, id: &str) -> Result<Component> {
        let spec = self.spec(id)?;
        let env = lock(&self.env);
        Ok(self.describe(&spec, &env))
    }

    /// "Use existing path": `path` is the library file or a folder containing it.
    pub fn set_path(&self, id: &str, path: &Path) -> Result<Component> {
        let spec = self.spec(id)?;
        if detect::library_in(path, &spec.detect.libraries).is_none() {
            return Err(DriverError::BadPath(format!(
                "{} does not contain {}",
                path.display(),
                spec.detect.libraries.join(" or ")
            )));
        }
        let mut env = lock(&self.env);
        env.overrides.insert(id.to_owned(), path.to_owned());
        write_overrides(&env.drivers_dir, &env.overrides)?;
        lock(&self.loaded).remove(id);
        Ok(self.describe(&spec, &env))
    }

    /// Remove what Switchyard installed or was pointed at. System packages stay.
    pub fn remove(&self, id: &str) -> Result<Component> {
        let spec = self.spec(id)?;
        lock(&self.loaded).remove(id);
        let mut env = lock(&self.env);
        if env.overrides.remove(id).is_some() {
            write_overrides(&env.drivers_dir, &env.overrides)?;
        }
        let dir = env.drivers_dir.join(id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
            info!(%id, "component removed");
        }
        Ok(self.describe(&spec, &env))
    }

    /// GSSAPI for Kerberos: the Driver Manager's `gssapi` component, or the system GSS
    /// framework on macOS.
    pub fn gssapi(&self) -> Result<crate::gssapi::Gssapi> {
        #[cfg(target_os = "macos")]
        let lib = {
            if let Some(l) = lock(&self.loaded).get(GSSAPI) {
                l.clone()
            } else {
                let l = Arc::new(open_library(Path::new(crate::gssapi::MACOS_FRAMEWORK))?);
                lock(&self.loaded).insert(GSSAPI.to_owned(), l.clone());
                l
            }
        };
        #[cfg(not(target_os = "macos"))]
        let lib = self.load(GSSAPI)?;
        crate::gssapi::Gssapi::new(lib)
    }

    /// Load a component's library, once; later calls share it.
    pub fn load(&self, id: &str) -> Result<Arc<Library>> {
        if let Some(l) = lock(&self.loaded).get(id) {
            return Ok(l.clone());
        }
        let c = self.component(id)?;
        let path = match &c.status {
            ComponentStatus::Installed {
                location,
                source: Source::UserPath | Source::AppManaged | Source::System,
                ..
            } => PathBuf::from(location),
            ComponentStatus::Installed { .. } => {
                return Err(DriverError::Load {
                    path: c.name,
                    message: "provided by the system; nothing to load".into(),
                });
            }
            _ => {
                return Err(DriverError::BadPath(format!("{} is not installed", c.name)));
            }
        };
        let lib = Arc::new(open_library(&path)?);
        lock(&self.loaded).insert(id.to_owned(), lib.clone());
        info!(%id, path = %path.display(), "component loaded");
        Ok(lib)
    }
}

#[allow(unsafe_code)]
pub(crate) fn open_library(path: &Path) -> Result<Library> {
    // SAFETY: loading a library runs its initializers. Only components the Driver Manager
    // found are loaded: system packages, archives verified against the signed manifest, or a
    // path the user chose for that component.
    unsafe { Library::new(path) }.map_err(|e| DriverError::Load {
        path: path.display().to_string(),
        message: e.to_string(),
    })
}

const OVERRIDES: &str = "paths.json";
/// The Kerberos / GSSAPI component's id in the manifest.
pub const GSSAPI: &str = "gssapi";

fn read_overrides(dir: &Path) -> HashMap<String, PathBuf> {
    match std::fs::read(dir.join(OVERRIDES)) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            warn!(error = %e, "ignoring unreadable driver paths file");
            HashMap::new()
        }),
        Err(_) => HashMap::new(),
    }
}

fn write_overrides(dir: &Path, map: &HashMap<String, PathBuf>) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_vec_pretty(map).map_err(|e| DriverError::Io(e.to_string()))?;
    std::fs::write(dir.join(OVERRIDES), json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::manifest::Os;

    fn registry(dir: &Path) -> Registry {
        let m = Manifest::parse(
            &std::fs::read(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/manifest.json"
            ))
            .unwrap(),
        )
        .unwrap();
        Registry::with_env(
            m,
            DetectEnv {
                drivers_dir: dir.join("drivers"),
                search_dirs: vec![],
                var: Box::new(|_| None),
                overrides: HashMap::new(),
                os: Os::Linux,
            },
            Box::new(|_| false),
        )
    }

    #[test]
    fn user_path_persists_and_remove_forgets_it() {
        let t = tempfile::tempdir().unwrap();
        let r = registry(t.path());
        let c = r.component("demo-client").unwrap();
        assert_eq!(c.status, ComponentStatus::Missing);
        assert!(matches!(c.plan, InstallPlan::Archive { .. }));
        assert!(c.license.unwrap().accept_required);

        let err = r.set_path("demo-client", t.path()).unwrap_err();
        assert!(matches!(err, DriverError::BadPath(_)), "{err}");
        let lib = t.path().join("opt/libdemo.so");
        std::fs::create_dir_all(lib.parent().unwrap()).unwrap();
        std::fs::write(&lib, b"not really a library").unwrap();
        let c = r.set_path("demo-client", lib.parent().unwrap()).unwrap();
        assert!(c.status.is_installed());

        // A fresh registry reads the saved path.
        let again = registry(t.path());
        assert!(
            again
                .component("demo-client")
                .unwrap()
                .status
                .is_installed()
        );
        // Not an ELF file: loading fails cleanly instead of crashing.
        let err = again.load("demo-client").unwrap_err();
        assert!(matches!(err, DriverError::Load { .. }), "{err}");

        let c = again.remove("demo-client").unwrap();
        assert_eq!(c.status, ComponentStatus::Missing);
        assert_eq!(
            registry(t.path()).component("demo-client").unwrap().status,
            ComponentStatus::Missing
        );
    }

    #[test]
    fn mirror_changes_the_plan() {
        let t = tempfile::tempdir().unwrap();
        let r = registry(t.path());
        r.set_mirror(Some("https://mirror.corp/drv".into()));
        let InstallPlan::Archive { url, .. } = r.component("demo-client").unwrap().plan else {
            panic!()
        };
        assert_eq!(url, "https://mirror.corp/drv/demo-2.1.tar.gz");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn loads_a_real_system_library() {
        // libc is always there; prove the libloading path works end to end.
        let t = tempfile::tempdir().unwrap();
        let mut m = Manifest::bundled();
        m.components[0].detect.libraries = vec!["libc.so.6".into()];
        let r = Registry::with_env(
            m,
            DetectEnv::system(t.path().join("drivers")),
            Box::new(|_| false),
        );
        let id = r.components()[0].id.clone();
        let a = r.load(&id).unwrap();
        let b = r.load(&id).unwrap();
        assert!(Arc::ptr_eq(&a, &b), "loaded once");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gssapi_comes_from_the_detected_system_library() {
        let t = tempfile::tempdir().unwrap();
        let r = Registry::with_env(
            Manifest::bundled(),
            DetectEnv::system(t.path().join("drivers")),
            Box::new(|_| false),
        );
        if r.component(GSSAPI).unwrap().status.is_installed() {
            r.gssapi()
                .expect("the detected libgssapi_krb5 loads and has the API");
        } else {
            // Not installed here: integrated auth reports it instead of crashing.
            assert!(r.gssapi().is_err());
        }
    }
}
