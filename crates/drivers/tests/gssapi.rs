//! Kerberos handshake through the runtime-loaded GSSAPI, against a throwaway MIT KDC:
//!
//!   eval "$(scripts/kerberos-test-kdc.sh start)"
//!   cargo test -p switchyard-drivers --test gssapi -- --ignored --test-threads 1
//!
//! The test plays the server with `gss_accept_sec_context` and the service keytab, so the
//! client side (what SQL Server logins use) goes through a full mutual-auth exchange.
#![allow(unsafe_code, clippy::unwrap_used, clippy::expect_used)]

use std::ffi::c_void;
use std::sync::Arc;

use libloading::Library;
use switchyard_drivers::gssapi::Gssapi;

#[repr(C)]
struct Buffer {
    length: usize,
    value: *mut c_void,
}

type Accept = unsafe extern "C" fn(
    *mut u32,
    *mut *mut c_void,
    *mut c_void,
    *mut Buffer,
    *mut c_void,
    *mut *mut c_void,
    *mut *mut c_void,
    *mut Buffer,
    *mut u32,
    *mut u32,
    *mut *mut c_void,
) -> u32;
type DisplayName =
    unsafe extern "C" fn(*mut u32, *mut c_void, *mut Buffer, *mut *mut c_void) -> u32;

fn library() -> Arc<Library> {
    Arc::new(unsafe { Library::new("libgssapi_krb5.so.2") }.expect("libgssapi_krb5.so.2"))
}

/// The server side of one step: returns its reply token and, when complete, the client name.
fn accept(lib: &Library, ctx: &mut *mut c_void, token: &[u8]) -> (u32, Vec<u8>, Option<String>) {
    let accept: libloading::Symbol<Accept> = unsafe { lib.get(b"gss_accept_sec_context") }.unwrap();
    let display: libloading::Symbol<DisplayName> = unsafe { lib.get(b"gss_display_name") }.unwrap();
    let mut minor = 0;
    let mut input = Buffer {
        length: token.len(),
        value: token.as_ptr() as *mut c_void,
    };
    let mut output = Buffer {
        length: 0,
        value: std::ptr::null_mut(),
    };
    let mut src: *mut c_void = std::ptr::null_mut();
    let major = unsafe {
        accept(
            &mut minor,
            ctx,
            std::ptr::null_mut(),
            &mut input,
            std::ptr::null_mut(),
            &mut src,
            std::ptr::null_mut(),
            &mut output,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    let reply = if output.value.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(output.value as *const u8, output.length) }.to_vec()
    };
    let name = (major == 0 && !src.is_null()).then(|| {
        let mut buf = Buffer {
            length: 0,
            value: std::ptr::null_mut(),
        };
        unsafe { display(&mut minor, src, &mut buf, std::ptr::null_mut()) };
        String::from_utf8_lossy(unsafe {
            std::slice::from_raw_parts(buf.value as *const u8, buf.length)
        })
        .into_owned()
    });
    (major, reply, name)
}

#[test]
#[ignore = "needs scripts/kerberos-test-kdc.sh"]
fn kerberos_handshake_with_the_users_ticket() {
    let spn = std::env::var("SWITCHYARD_TEST_SPN").expect("run scripts/kerberos-test-kdc.sh");
    let lib = library();
    let gss = Gssapi::new(lib.clone()).unwrap();

    let mut client = gss.client(&spn).unwrap();
    let first = client.step(None).unwrap().expect("an initial token");
    // An RFC 2743 initial context token: [APPLICATION 0] with the Kerberos mechanism OID.
    assert_eq!(first[0], 0x60);
    assert!(!client.is_complete(), "mutual auth waits for the server");

    let mut server_ctx: *mut c_void = std::ptr::null_mut();
    let (major, reply, name) = accept(&lib, &mut server_ctx, &first);
    assert_eq!(major, 0, "the service accepted the ticket");
    assert_eq!(name.as_deref(), Some("swy@SWITCHYARD.TEST"));

    let last = client.step(Some(&reply)).unwrap();
    assert!(
        client.is_complete(),
        "the server's reply completes mutual auth"
    );
    assert!(last.unwrap_or_default().is_empty());
}

#[test]
#[ignore = "needs scripts/kerberos-test-kdc.sh"]
fn unknown_service_is_a_readable_error() {
    let gss = Gssapi::new(library()).unwrap();
    let mut client = gss.client("MSSQLSvc/nowhere.switchyard.test:1433").unwrap();
    let err = client.step(None).expect_err("the KDC has no such service");
    assert!(
        err.starts_with("Kerberos could not sign in to the server"),
        "{err}"
    );
    assert!(err.to_lowercase().contains("not found"), "{err}");
}

#[test]
#[ignore = "needs scripts/kerberos-test-kdc.sh"]
fn no_ticket_is_a_readable_error() {
    let dir = tempfile::tempdir().unwrap();
    let saved = std::env::var_os("KRB5CCNAME");
    // Run with --test-threads 1: the cache variable is process-wide.
    unsafe { std::env::set_var("KRB5CCNAME", format!("FILE:{}/empty", dir.path().display())) };
    let gss = Gssapi::new(library()).unwrap();
    let spn = std::env::var("SWITCHYARD_TEST_SPN").unwrap();
    let result = gss.client(&spn).unwrap().step(None);
    match saved {
        Some(v) => unsafe { std::env::set_var("KRB5CCNAME", v) },
        None => unsafe { std::env::remove_var("KRB5CCNAME") },
    }
    let err = result.expect_err("no ticket cache");
    assert!(
        err.contains("credentials") || err.contains("cache"),
        "{err}"
    );
}
