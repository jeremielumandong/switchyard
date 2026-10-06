//! Kerberos client contexts through GSSAPI loaded at runtime: MIT `libgssapi_krb5` on
//! Linux (found and installed by the Driver Manager), the system GSS framework on macOS.
//! Switchyard never links GSSAPI at build time, so the app starts without it; only
//! integrated authentication needs it.
//!
//! Only the client side (`gss_init_sec_context`) is used; credentials come from the user's
//! Kerberos ticket cache (`kinit`).
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::sync::Arc;

use libloading::Library;

use crate::error::{DriverError, Result};

type OmUint32 = u32;

// Apple's GSS headers pack these structures to 2 bytes (`#pragma pack(push, 2)`), which
// moves the OID's `elements` pointer to offset 4. MIT's Linux headers use natural layout.
#[cfg_attr(target_os = "macos", repr(C, packed(2)))]
#[cfg_attr(not(target_os = "macos"), repr(C))]
struct OidDesc {
    length: OmUint32,
    elements: *mut c_void,
}

#[cfg_attr(target_os = "macos", repr(C, packed(2)))]
#[cfg_attr(not(target_os = "macos"), repr(C))]
struct BufferDesc {
    length: usize,
    value: *mut c_void,
}

impl BufferDesc {
    fn empty() -> Self {
        Self {
            length: 0,
            value: std::ptr::null_mut(),
        }
    }
}

type Name = *mut c_void;
type Ctx = *mut c_void;

type ImportName =
    unsafe extern "C" fn(*mut OmUint32, *mut BufferDesc, *mut OidDesc, *mut Name) -> OmUint32;
type InitSecContext = unsafe extern "C" fn(
    minor: *mut OmUint32,
    cred: *mut c_void,
    ctx: *mut Ctx,
    target: Name,
    mech: *mut OidDesc,
    flags: OmUint32,
    time_req: OmUint32,
    bindings: *mut c_void,
    input: *mut BufferDesc,
    actual_mech: *mut *mut OidDesc,
    output: *mut BufferDesc,
    ret_flags: *mut OmUint32,
    time_rec: *mut OmUint32,
) -> OmUint32;
type ReleaseBuffer = unsafe extern "C" fn(*mut OmUint32, *mut BufferDesc) -> OmUint32;
type ReleaseName = unsafe extern "C" fn(*mut OmUint32, *mut Name) -> OmUint32;
type DeleteSecContext = unsafe extern "C" fn(*mut OmUint32, *mut Ctx, *mut BufferDesc) -> OmUint32;
type DisplayStatus = unsafe extern "C" fn(
    *mut OmUint32,
    OmUint32,
    i32,
    *mut OidDesc,
    *mut OmUint32,
    *mut BufferDesc,
) -> OmUint32;

const GSS_S_COMPLETE: OmUint32 = 0;
const GSS_S_CONTINUE_NEEDED: OmUint32 = 1;
const GSS_C_MUTUAL_FLAG: OmUint32 = 2;
const GSS_C_SEQUENCE_FLAG: OmUint32 = 8;
const GSS_C_GSS_CODE: i32 = 1;
const GSS_C_MECH_CODE: i32 = 2;

/// 1.2.840.113554.1.2.2 — the Kerberos 5 mechanism.
const KRB5_MECH: [u8; 9] = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x12, 0x01, 0x02, 0x02];
/// 1.2.840.113554.1.2.2.1 — a Kerberos principal name (`MSSQLSvc/host:1433`).
const KRB5_PRINCIPAL_NAME: [u8; 10] = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x12, 0x01, 0x02, 0x02, 0x01];

/// The system GSS framework on macOS (in the dyld shared cache, so no file to check).
#[cfg(target_os = "macos")]
pub const MACOS_FRAMEWORK: &str = "/System/Library/Frameworks/GSS.framework/GSS";

/// A loaded GSSAPI library.
#[derive(Clone)]
pub struct Gssapi(Arc<Api>);

struct Api {
    import_name: ImportName,
    init_sec_context: InitSecContext,
    release_buffer: ReleaseBuffer,
    release_name: ReleaseName,
    delete_sec_context: DeleteSecContext,
    display_status: DisplayStatus,
    /// Keeps the functions above valid.
    _lib: Arc<Library>,
}

impl std::fmt::Debug for Gssapi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Gssapi")
    }
}

fn symbol<T: Copy>(lib: &Library, name: &str) -> Result<T> {
    // SAFETY: `T` is the C signature of `name` from RFC 2744 (gssapi.h); the pointer is
    // only used while `Api` holds the library.
    unsafe { lib.get::<T>(name.as_bytes()) }
        .map(|s| *s)
        .map_err(|e| DriverError::Load {
            path: "GSSAPI".into(),
            message: format!("{name}: {e}"),
        })
}

impl Gssapi {
    /// Resolve the GSSAPI functions in `lib`.
    pub fn new(lib: Arc<Library>) -> Result<Self> {
        Ok(Self(Arc::new(Api {
            import_name: symbol(&lib, "gss_import_name")?,
            init_sec_context: symbol(&lib, "gss_init_sec_context")?,
            release_buffer: symbol(&lib, "gss_release_buffer")?,
            release_name: symbol(&lib, "gss_release_name")?,
            delete_sec_context: symbol(&lib, "gss_delete_sec_context")?,
            display_status: symbol(&lib, "gss_display_status")?,
            _lib: lib,
        })))
    }

    /// Start a Kerberos client context for `spn` (e.g. `MSSQLSvc/db.corp:1433`) with the
    /// user's default credentials.
    pub fn client(&self, spn: &str) -> Result<KerberosContext, String> {
        let api = &self.0;
        let mut minor = 0;
        let mut buf = BufferDesc {
            length: spn.len(),
            value: spn.as_ptr() as *mut c_void,
        };
        let mut name_type = OidDesc {
            length: KRB5_PRINCIPAL_NAME.len() as OmUint32,
            elements: KRB5_PRINCIPAL_NAME.as_ptr() as *mut c_void,
        };
        let mut name: Name = std::ptr::null_mut();
        // SAFETY: the buffer and OID point into live memory for the call; GSSAPI copies
        // the name and returns a handle we release in `Drop`.
        let major = unsafe { (api.import_name)(&mut minor, &mut buf, &mut name_type, &mut name) };
        if major != GSS_S_COMPLETE {
            return Err(self.error("import the server name", major, minor));
        }
        Ok(KerberosContext {
            api: self.clone(),
            name,
            ctx: std::ptr::null_mut(),
            done: false,
        })
    }

    fn status(&self, code: OmUint32, kind: i32) -> Vec<String> {
        let api = &self.0;
        let mut out = Vec::new();
        let mut more: OmUint32 = 0;
        loop {
            let mut minor = 0;
            let mut text = BufferDesc::empty();
            // SAFETY: GSSAPI fills `text`, which is released right after copying.
            let major = unsafe {
                (api.display_status)(
                    &mut minor,
                    code,
                    kind,
                    std::ptr::null_mut(),
                    &mut more,
                    &mut text,
                )
            };
            if major != GSS_S_COMPLETE {
                break;
            }
            out.push(take(api, text).map(|b| String::from_utf8_lossy(&b).into_owned()));
            if more == 0 || out.len() > 8 {
                break;
            }
        }
        out.into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect()
    }

    fn error(&self, what: &str, major: OmUint32, minor: OmUint32) -> String {
        let mut parts = self.status(major, GSS_C_GSS_CODE);
        if minor != 0 {
            parts.extend(self.status(minor, GSS_C_MECH_CODE));
        }
        if parts.is_empty() {
            parts.push(format!("GSSAPI status {major:#x}/{minor}"));
        }
        format!("Kerberos could not {what}: {}", parts.join("; "))
    }
}

/// Copy a GSSAPI-owned buffer and release it.
fn take(api: &Api, mut buf: BufferDesc) -> Option<Vec<u8>> {
    let (length, value) = (buf.length, buf.value);
    let bytes = (!value.is_null() && length > 0).then(|| {
        // SAFETY: GSSAPI returned `length` readable bytes at `value`.
        unsafe { std::slice::from_raw_parts(value as *const u8, length) }.to_vec()
    });
    let mut minor = 0;
    // SAFETY: the buffer came from GSSAPI and is released once.
    unsafe { (api.release_buffer)(&mut minor, &mut buf) };
    bytes
}

/// One Kerberos handshake with a server.
pub struct KerberosContext {
    api: Gssapi,
    name: Name,
    ctx: Ctx,
    done: bool,
}

// SAFETY: the handles are only used through `&mut self`, one call at a time; GSSAPI
// contexts may move between threads as long as they aren't used concurrently.
unsafe impl Send for KerberosContext {}

impl KerberosContext {
    /// Feed the server's token (none at first) and get the next token to send.
    pub fn step(&mut self, input: Option<&[u8]>) -> Result<Option<Vec<u8>>, String> {
        if self.done {
            return Ok(None);
        }
        let api = &self.api.0;
        let mut minor = 0;
        let mut mech = OidDesc {
            length: KRB5_MECH.len() as OmUint32,
            elements: KRB5_MECH.as_ptr() as *mut c_void,
        };
        let mut input_buf = match input {
            Some(b) => BufferDesc {
                length: b.len(),
                value: b.as_ptr() as *mut c_void,
            },
            None => BufferDesc::empty(),
        };
        let mut output = BufferDesc::empty();
        // SAFETY: every pointer refers to live memory for the duration of the call; the
        // context and output buffer are owned by GSSAPI and released by us.
        let major = unsafe {
            (api.init_sec_context)(
                &mut minor,
                std::ptr::null_mut(),
                &mut self.ctx,
                self.name,
                &mut mech,
                GSS_C_MUTUAL_FLAG | GSS_C_SEQUENCE_FLAG,
                0,
                std::ptr::null_mut(),
                &mut input_buf,
                std::ptr::null_mut(),
                &mut output,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        let token = take(api, output);
        match major {
            GSS_S_COMPLETE => {
                self.done = true;
                Ok(token)
            }
            GSS_S_CONTINUE_NEEDED => Ok(token),
            _ => Err(self.api.error("sign in to the server", major, minor)),
        }
    }

    /// Whether the handshake finished.
    pub fn is_complete(&self) -> bool {
        self.done
    }
}

impl Drop for KerberosContext {
    fn drop(&mut self) {
        let api = &self.api.0;
        let mut minor = 0;
        // SAFETY: handles were created by this library and are released exactly once.
        unsafe {
            if !self.ctx.is_null() {
                (api.delete_sec_context)(&mut minor, &mut self.ctx, std::ptr::null_mut());
            }
            if !self.name.is_null() {
                (api.release_name)(&mut minor, &mut self.name);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oid_layout_matches_the_platform_headers() {
        if cfg!(target_os = "macos") {
            assert_eq!(std::mem::size_of::<OidDesc>(), 12);
        } else {
            assert_eq!(std::mem::size_of::<OidDesc>(), 16);
        }
        assert_eq!(std::mem::size_of::<BufferDesc>(), 16);
    }

    #[test]
    fn a_library_without_gssapi_is_refused() {
        // libc is always there and has no gss_* functions.
        let lib =
            crate::registry::open_library(std::path::Path::new(if cfg!(target_os = "macos") {
                "/usr/lib/libSystem.B.dylib"
            } else {
                "libc.so.6"
            }));
        let Ok(lib) = lib else { return };
        let err = Gssapi::new(Arc::new(lib)).expect_err("libc has no GSSAPI");
        assert!(err.to_string().contains("gss_import_name"), "{err}");
    }
}
