//! Windows (MSVC): embed `packaging/icons/switchyard.ico` as the executable's icon resource
//! (group icon id 1, which GPUI loads for the window and Windows shows for the file).
//!
//! The `.res` file is written here directly (its format is small and stable), so no
//! resource compiler or extra build dependency is needed; `link.exe` takes `.res` inputs.
//! Other targets do nothing: macOS and Linux take the icon from the bundle / desktop file.

use std::path::{Path, PathBuf};

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const LANG_EN_US: u16 = 0x0409;
const MOVEABLE_DISCARDABLE: u16 = 0x1010;
const MOVEABLE_PURE_DISCARDABLE: u16 = 0x1030;

fn main() {
    let ico: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "..",
        "..",
        "packaging",
        "icons",
        "switchyard.ico",
    ]
    .iter()
    .collect();
    println!("cargo:rerun-if-changed={}", ico.display());
    println!("cargo:rerun-if-changed=build.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os != "windows" || env != "msvc" {
        return;
    }
    let Some(out) = std::env::var_os("OUT_DIR").map(PathBuf::from) else {
        return;
    };
    match write_res(&ico, &out.join("switchyard-icon.res")) {
        Ok(res) => println!("cargo:rustc-link-arg-bin=switchyard={}", res.display()),
        // A missing icon must not fail the build; the window falls back to the default icon.
        Err(e) => println!("cargo:warning=no Windows icon resource: {e}"),
    }
}

/// One resource entry with numeric type and name.
fn push_resource(res: &mut Vec<u8>, kind: u16, name: u16, flags: u16, data: &[u8]) {
    res.extend_from_slice(&(data.len() as u32).to_le_bytes());
    res.extend_from_slice(&32u32.to_le_bytes()); // header size
    res.extend_from_slice(&[0xFF, 0xFF]);
    res.extend_from_slice(&kind.to_le_bytes());
    res.extend_from_slice(&[0xFF, 0xFF]);
    res.extend_from_slice(&name.to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes()); // data version
    res.extend_from_slice(&flags.to_le_bytes());
    res.extend_from_slice(&LANG_EN_US.to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes()); // version
    res.extend_from_slice(&0u32.to_le_bytes()); // characteristics
    res.extend_from_slice(data);
    while !res.len().is_multiple_of(4) {
        res.push(0);
    }
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]))
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(i)?,
        *b.get(i + 1)?,
        *b.get(i + 2)?,
        *b.get(i + 3)?,
    ]))
}

/// Convert an `.ico` file into a `.res` holding its images (RT_ICON 1..n) and a group
/// (RT_GROUP_ICON 1) that lists them.
fn write_res(ico_path: &Path, out: &Path) -> Result<PathBuf, String> {
    let ico = std::fs::read(ico_path).map_err(|e| format!("{}: {e}", ico_path.display()))?;
    let bad = || format!("{} is not a valid .ico file", ico_path.display());
    if u16_at(&ico, 2) != Some(1) {
        return Err(bad());
    }
    let count = u16_at(&ico, 4).ok_or_else(bad)?;
    // The empty resource every .res file starts with.
    let mut res = Vec::new();
    res.extend_from_slice(&0u32.to_le_bytes());
    res.extend_from_slice(&32u32.to_le_bytes());
    res.extend_from_slice(&[0xFF, 0xFF, 0, 0, 0xFF, 0xFF, 0, 0]);
    res.extend_from_slice(&[0u8; 16]);
    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes());
    group.extend_from_slice(&1u16.to_le_bytes());
    group.extend_from_slice(&count.to_le_bytes());
    for n in 0..count {
        let e = 6 + 16 * usize::from(n);
        let entry = ico.get(e..e + 12).ok_or_else(bad)?;
        let size = u32_at(&ico, e + 8).ok_or_else(bad)? as usize;
        let offset = u32_at(&ico, e + 12).ok_or_else(bad)? as usize;
        let image = ico.get(offset..offset + size).ok_or_else(bad)?;
        let id = n + 1;
        push_resource(&mut res, RT_ICON, id, MOVEABLE_DISCARDABLE, image);
        group.extend_from_slice(entry);
        group.extend_from_slice(&id.to_le_bytes());
    }
    push_resource(
        &mut res,
        RT_GROUP_ICON,
        1,
        MOVEABLE_PURE_DISCARDABLE,
        &group,
    );
    std::fs::write(out, &res).map_err(|e| format!("{}: {e}", out.display()))?;
    Ok(out.to_path_buf())
}
