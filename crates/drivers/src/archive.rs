//! Unpacking `.tar.gz` driver archives into a folder, refusing anything that could write
//! outside it (absolute paths, `..`, links pointing out).
//!
//! A small ustar/GNU/pax reader: regular files, folders and relative symlinks (vendor
//! libraries use them for sonames). Hard links, devices and FIFOs are refused.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;

use crate::error::{DriverError, Result};

const BLOCK: usize = 512;
/// Refuse archives that unpack to more than this (a corrupted size field or a bomb).
const MAX_TOTAL: u64 = 4 << 30;

fn bad(msg: impl Into<String>) -> DriverError {
    DriverError::Archive(msg.into())
}

fn octal(field: &[u8]) -> Result<u64> {
    let s: String = field
        .iter()
        .take_while(|b| **b != 0)
        .map(|b| *b as char)
        .collect();
    let s = s.trim();
    if s.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(s, 8).map_err(|_| bad(format!("bad number field {s:?}")))
}

fn text(field: &[u8]) -> String {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// `a/b/c` → a relative path with only normal parts.
fn safe_relative(name: &str) -> Result<PathBuf> {
    let p = Path::new(name);
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return Err(bad(format!("unsafe path in archive: {name}"))),
        }
    }
    if out.as_os_str().is_empty() {
        return Err(bad("empty path in archive"));
    }
    Ok(out)
}

/// A symlink at `at` (relative to the root) pointing to `target` must stay inside.
fn link_stays_inside(at: &Path, target: &str) -> bool {
    let t = Path::new(target);
    if t.is_absolute() {
        return false;
    }
    let mut depth = at.components().count() as i64 - 1;
    for c in t.components() {
        match c {
            Component::ParentDir => depth -= 1,
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            _ => return false,
        }
        if depth < 0 {
            return false;
        }
    }
    true
}

/// Entries must not be written through a symlink unpacked earlier, or a link to `..`
/// followed by `link/../..` would escape.
fn no_link_on_the_way(dest: &Path, rel: &Path) -> Result<()> {
    let mut p = dest.to_owned();
    let parts: Vec<_> = rel.components().collect();
    for c in &parts[..parts.len().saturating_sub(1)] {
        p.push(c);
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(bad(format!("{} is written through a link", rel.display())));
        }
    }
    Ok(())
}

fn read_exact_or_eof(r: &mut impl Read, buf: &mut [u8]) -> Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 if filled == 0 => return Ok(false),
            0 => return Err(bad("archive ends in the middle of a block")),
            n => filled += n,
        }
    }
    Ok(true)
}

fn read_body(r: &mut impl Read, size: u64) -> Result<Vec<u8>> {
    let mut data = vec![0u8; size as usize];
    r.read_exact(&mut data)?;
    skip_padding(r, size)?;
    Ok(data)
}

fn skip_padding(r: &mut impl Read, size: u64) -> Result<()> {
    let pad = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;
    std::io::copy(&mut r.take(pad), &mut std::io::sink())?;
    Ok(())
}

fn pax_path(data: &[u8]) -> Option<String> {
    // Records: "<len> key=value\n".
    let s = String::from_utf8_lossy(data);
    s.lines().find_map(|l| {
        let (_, kv) = l.split_once(' ')?;
        kv.strip_prefix("path=").map(str::to_owned)
    })
}

/// Unpack a gzip-compressed tar stream into `dest` (created if missing).
pub fn extract_tar_gz(reader: impl Read, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    let mut r = GzDecoder::new(reader);
    let mut header = [0u8; BLOCK];
    let mut long_name: Option<String> = None;
    let mut total = 0u64;
    loop {
        if !read_exact_or_eof(&mut r, &mut header)? {
            break;
        }
        if header.iter().all(|b| *b == 0) {
            break; // end-of-archive marker
        }
        let size = octal(&header[124..136])?;
        total = total.saturating_add(size);
        if total > MAX_TOTAL {
            return Err(bad("archive unpacks to more than 4 GB"));
        }
        let kind = header[156];
        let mut name = text(&header[0..100]);
        if &header[257..262] == b"ustar" {
            let prefix = text(&header[345..500]);
            if !prefix.is_empty() {
                name = format!("{prefix}/{name}");
            }
        }
        if let Some(n) = long_name.take() {
            name = n;
        }
        match kind {
            b'L' => {
                let data = read_body(&mut r, size)?;
                long_name = Some(text(&data));
                continue;
            }
            b'x' => {
                let data = read_body(&mut r, size)?;
                long_name = pax_path(&data);
                continue;
            }
            b'g' => {
                read_body(&mut r, size)?;
                continue;
            }
            _ => {}
        }
        let rel = safe_relative(&name)?;
        no_link_on_the_way(dest, &rel)?;
        let path = dest.join(&rel);
        match kind {
            b'0' | 0 | b'7' => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut out = std::fs::File::create(&path)?;
                std::io::copy(&mut (&mut r).take(size), &mut out)?;
                skip_padding(&mut r, size)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let mode = octal(&header[100..108])? as u32 & 0o755;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode | 0o600))?;
                }
            }
            b'5' => {
                std::fs::create_dir_all(&path)?;
                skip_padding(&mut r, size)?;
            }
            b'2' => {
                let target = text(&header[157..257]);
                if !link_stays_inside(&rel, &target) {
                    return Err(bad(format!("link {name} points outside the archive")));
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                #[cfg(unix)]
                std::os::unix::fs::symlink(&target, &path)?;
                #[cfg(not(unix))]
                {
                    // Windows: copy instead of linking, when the target exists already.
                    let src = path.parent().map(|p| p.join(&target));
                    if let Some(src) = src.filter(|s| s.is_file()) {
                        std::fs::copy(src, &path)?;
                    }
                }
            }
            other => {
                return Err(bad(format!(
                    "unsupported entry type {:?} for {name}",
                    other as char
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    /// Build a `.tar.gz` from (name, kind, body-or-link-target).
    pub(crate) fn tar_gz(entries: &[(&str, u8, &[u8])]) -> Vec<u8> {
        let mut tar = Vec::new();
        for (name, kind, body) in entries {
            let mut h = [0u8; BLOCK];
            h[..name.len()].copy_from_slice(name.as_bytes());
            let size = if *kind == b'2' { 0 } else { body.len() };
            h[100..107].copy_from_slice(b"0000755");
            h[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
            h[156] = *kind;
            if *kind == b'2' {
                h[157..157 + body.len()].copy_from_slice(body);
            }
            h[257..263].copy_from_slice(b"ustar\0");
            h[148..156].copy_from_slice(b"        ");
            let sum: u32 = h.iter().map(|b| *b as u32).sum();
            h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
            tar.extend_from_slice(&h);
            if *kind != b'2' {
                tar.extend_from_slice(body);
                tar.resize(tar.len().div_ceil(BLOCK) * BLOCK, 0);
            }
        }
        tar.extend_from_slice(&[0u8; BLOCK * 2]);
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        gz.write_all(&tar).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn unpacks_files_dirs_and_inner_links() {
        let t = tempfile::tempdir().unwrap();
        let data = tar_gz(&[
            ("demo/", b'5', b""),
            ("demo/libdemo.so.2.1", b'0', b"ELF..."),
            ("demo/libdemo.so", b'2', b"libdemo.so.2.1"),
        ]);
        extract_tar_gz(data.as_slice(), t.path()).unwrap();
        assert_eq!(
            std::fs::read(t.path().join("demo/libdemo.so.2.1")).unwrap(),
            b"ELF..."
        );
        #[cfg(unix)]
        assert_eq!(
            std::fs::read(t.path().join("demo/libdemo.so")).unwrap(),
            b"ELF..."
        );
    }

    #[test]
    fn refuses_escapes() {
        for entries in [
            vec![("../evil", b'0', &b"x"[..])],
            vec![("/etc/evil", b'0', &b"x"[..])],
            vec![("demo/link", b'2', &b"../../etc/passwd"[..])],
            vec![("demo/link", b'2', &b"/etc/passwd"[..])],
            vec![("demo/hard", b'1', &b""[..])],
        ] {
            let t = tempfile::tempdir().unwrap();
            let err = extract_tar_gz(tar_gz(&entries).as_slice(), t.path()).unwrap_err();
            assert!(matches!(err, DriverError::Archive(_)), "{err}");
        }
        // Writing through a link unpacked earlier (links are only created on Unix).
        #[cfg(unix)]
        {
            let t = tempfile::tempdir().unwrap();
            let entries = [("d/l", b'2', &b".."[..]), ("d/l/x", b'0', &b"x"[..])];
            let err = extract_tar_gz(tar_gz(&entries).as_slice(), t.path()).unwrap_err();
            assert!(matches!(err, DriverError::Archive(_)), "{err}");
        }
    }

    #[test]
    fn link_check() {
        assert!(link_stays_inside(Path::new("a/b/l"), "../c"));
        assert!(!link_stays_inside(Path::new("a/l"), "../../c"));
        assert!(!link_stays_inside(Path::new("l"), "../c"));
    }
}
