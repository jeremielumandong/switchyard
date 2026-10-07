//! Unpacking `.tar.gz` and `.zip` driver archives into a folder, refusing anything that
//! could write outside it (absolute paths, `..`, links pointing out).
//!
//! A small ustar/GNU/pax reader: regular files, folders and relative symlinks (vendor
//! libraries use them for sonames). Hard links, devices and FIFOs are refused. The zip
//! reader takes stored and deflated entries and Unix symlinks (Oracle Instant Client);
//! encrypted and zip64 archives are refused.

use std::io::{Read, Seek, SeekFrom};
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

/// Unpack `archive` into `dest`, choosing the format from its first bytes.
pub fn extract(archive: &Path, dest: &Path) -> Result<()> {
    let mut f = std::fs::File::open(archive)?;
    let mut magic = [0u8; 4];
    let n = f.read(&mut magic)?;
    f.seek(SeekFrom::Start(0))?;
    if n == 4 && magic == *b"PK\x03\x04" {
        extract_zip(f, dest)
    } else {
        extract_tar_gz(std::io::BufReader::new(f), dest)
    }
}

fn u16_at(b: &[u8], at: usize) -> Result<u16> {
    b.get(at..at + 2)
        .map(|x| u16::from_le_bytes([x[0], x[1]]))
        .ok_or_else(|| bad("truncated zip record"))
}

fn u32_at(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 4)
        .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
        .ok_or_else(|| bad("truncated zip record"))
}

/// One central-directory entry.
struct ZipEntry {
    name: String,
    method: u16,
    crc: u32,
    compressed: u64,
    size: u64,
    /// Unix mode, when the archive was made on Unix.
    mode: Option<u32>,
    local_offset: u64,
}

fn central_directory<R: Read + Seek>(r: &mut R) -> Result<Vec<ZipEntry>> {
    const EOCD: u32 = 0x0605_4b50;
    let len = r.seek(SeekFrom::End(0))?;
    let tail_len = len.min(22 + 0xFFFF);
    r.seek(SeekFrom::Start(len - tail_len))?;
    let mut tail = vec![0u8; tail_len as usize];
    r.read_exact(&mut tail)?;
    let at = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| u32_at(&tail, i).is_ok_and(|v| v == EOCD))
        .ok_or_else(|| bad("not a zip archive (no end of central directory)"))?;
    let count = u16_at(&tail, at + 10)?;
    let cd_size = u32_at(&tail, at + 12)?;
    let cd_offset = u32_at(&tail, at + 16)?;
    if count == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF {
        return Err(bad("zip64 archives are not supported"));
    }
    r.seek(SeekFrom::Start(u64::from(cd_offset)))?;
    let mut cd = vec![0u8; cd_size as usize];
    r.read_exact(&mut cd)?;
    let mut out = Vec::with_capacity(count as usize);
    let mut p = 0usize;
    for _ in 0..count {
        if u32_at(&cd, p)? != 0x0201_4b50 {
            return Err(bad("corrupt zip central directory"));
        }
        let made_by_os = cd.get(p + 5).copied().unwrap_or(0);
        let flags = u16_at(&cd, p + 8)?;
        if flags & 1 != 0 {
            return Err(bad("encrypted zip entries are not supported"));
        }
        let name_len = usize::from(u16_at(&cd, p + 28)?);
        let extra_len = usize::from(u16_at(&cd, p + 30)?);
        let comment_len = usize::from(u16_at(&cd, p + 32)?);
        let external = u32_at(&cd, p + 38)?;
        let name = cd
            .get(p + 46..p + 46 + name_len)
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .ok_or_else(|| bad("truncated zip entry name"))?;
        out.push(ZipEntry {
            name,
            method: u16_at(&cd, p + 10)?,
            crc: u32_at(&cd, p + 16)?,
            compressed: u64::from(u32_at(&cd, p + 20)?),
            size: u64::from(u32_at(&cd, p + 24)?),
            mode: (made_by_os == 3).then_some(external >> 16),
            local_offset: u64::from(u32_at(&cd, p + 42)?),
        });
        p += 46 + name_len + extra_len + comment_len;
    }
    Ok(out)
}

/// The entry's bytes, inflated and checked against its CRC-32.
fn zip_body<R: Read + Seek>(r: &mut R, e: &ZipEntry, out: &mut dyn std::io::Write) -> Result<()> {
    r.seek(SeekFrom::Start(e.local_offset))?;
    let mut local = [0u8; 30];
    r.read_exact(&mut local)?;
    if u32_at(&local, 0)? != 0x0403_4b50 {
        return Err(bad(format!("corrupt local header for {}", e.name)));
    }
    let skip = u64::from(u16_at(&local, 26)?) + u64::from(u16_at(&local, 28)?);
    r.seek(SeekFrom::Current(skip as i64))?;
    let raw = (&mut *r).take(e.compressed);
    let mut reader: Box<dyn Read + '_> = match e.method {
        0 => Box::new(raw),
        8 => Box::new(flate2::read::DeflateDecoder::new(raw)),
        m => {
            return Err(bad(format!(
                "unsupported zip compression {m} for {}",
                e.name
            )));
        }
    };
    let mut crc = flate2::Crc::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut written = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        written += n as u64;
        if written > e.size {
            return Err(bad(format!("{} is larger than recorded", e.name)));
        }
        crc.update(&buf[..n]);
        out.write_all(&buf[..n])?;
    }
    if written != e.size || crc.sum() != e.crc {
        return Err(bad(format!("{} is corrupt (size or CRC mismatch)", e.name)));
    }
    Ok(())
}

/// Unpack a zip archive into `dest` (created if missing).
pub fn extract_zip<R: Read + Seek>(mut r: R, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    let entries = central_directory(&mut r)?;
    let total: u64 = entries.iter().map(|e| e.size).sum();
    if total > MAX_TOTAL {
        return Err(bad("archive unpacks to more than 4 GiB"));
    }
    for e in &entries {
        let is_dir = e.name.ends_with('/') || e.mode.is_some_and(|m| m & 0o170000 == 0o040000);
        let is_link = e.mode.is_some_and(|m| m & 0o170000 == 0o120000);
        let rel = safe_relative(e.name.trim_end_matches('/'))?;
        no_link_on_the_way(dest, &rel)?;
        let path = dest.join(&rel);
        if is_dir {
            std::fs::create_dir_all(&path)?;
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if is_link {
            if e.size > 4096 {
                return Err(bad(format!("link {} has an implausible target", e.name)));
            }
            let mut target = Vec::new();
            zip_body(&mut r, e, &mut target)?;
            let target = String::from_utf8_lossy(&target).into_owned();
            if !link_stays_inside(&rel, &target) {
                return Err(bad(format!("link {} points outside the archive", e.name)));
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &path)?;
            #[cfg(not(unix))]
            {
                let src = path.parent().map(|p| p.join(&target));
                if let Some(src) = src.filter(|s| s.is_file()) {
                    std::fs::copy(src, &path)?;
                }
            }
            continue;
        }
        let mut out = std::fs::File::create(&path)?;
        zip_body(&mut r, e, &mut out)?;
        #[cfg(unix)]
        if let Some(mode) = e.mode {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(
                &path,
                std::fs::Permissions::from_mode((mode & 0o755) | 0o600),
            )?;
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

    /// A zip with the given entries: (name, unix mode, data, deflate?).
    pub(crate) fn zip(entries: &[(&str, u32, &[u8], bool)]) -> Vec<u8> {
        use flate2::write::DeflateEncoder;
        let mut out = Vec::new();
        let mut cd = Vec::new();
        for (name, mode, data, deflate) in entries {
            let mut crc = flate2::Crc::new();
            crc.update(data);
            let body = if *deflate {
                let mut e = DeflateEncoder::new(Vec::new(), Compression::fast());
                e.write_all(data).unwrap();
                e.finish().unwrap()
            } else {
                data.to_vec()
            };
            let offset = out.len() as u32;
            let method: u16 = if *deflate { 8 } else { 0 };
            // Local header, sizes in a data descriptor like Oracle's archives.
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&[20, 0, 8, 0]);
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&[0; 12]);
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&body);
            out.extend_from_slice(&0x0807_4b50u32.to_le_bytes());
            out.extend_from_slice(&crc.sum().to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());

            cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            cd.extend_from_slice(&[20, 3, 20, 0, 8, 0]);
            cd.extend_from_slice(&method.to_le_bytes());
            cd.extend_from_slice(&[0; 4]);
            cd.extend_from_slice(&crc.sum().to_le_bytes());
            cd.extend_from_slice(&(body.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
            cd.extend_from_slice(&[0; 8]);
            cd.extend_from_slice(&(mode << 16).to_le_bytes());
            cd.extend_from_slice(&offset.to_le_bytes());
            cd.extend_from_slice(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&cd);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        let n = entries.len() as u16;
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&(cd.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&[0; 2]);
        out
    }

    #[test]
    fn unzips_files_dirs_and_links() {
        let dir = tempfile::tempdir().unwrap();
        let big = vec![b'x'; 100_000];
        let z = zip(&[
            ("ic/", 0o040755, b"", false),
            ("ic/libclntsh.so.23.1", 0o100755, &big, true),
            ("ic/libclntsh.so", 0o120777, b"libclntsh.so.23.1", false),
            ("ic/README", 0o100644, b"hello", false),
        ]);
        extract_zip(std::io::Cursor::new(z), dir.path()).unwrap();
        let lib = dir.path().join("ic/libclntsh.so");
        assert_eq!(std::fs::read(&lib).unwrap().len(), 100_000);
        assert_eq!(
            std::fs::read(dir.path().join("ic/README")).unwrap(),
            b"hello"
        );
        #[cfg(unix)]
        assert!(
            std::fs::symlink_metadata(&lib)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn unzip_refuses_escapes_and_corruption() {
        for entries in [
            vec![("../evil", 0o100644, &b"x"[..], false)],
            vec![("/abs", 0o100644, &b"x"[..], false)],
            vec![("ic/link", 0o120777, &b"../../etc/passwd"[..], false)],
        ] {
            let dir = tempfile::tempdir().unwrap();
            let err = extract_zip(std::io::Cursor::new(zip(&entries)), dir.path()).unwrap_err();
            assert!(matches!(err, DriverError::Archive(_)), "{err}");
        }
        let mut z = zip(&[("a", 0o100644, b"hello world", false)]);
        // Flip a byte of the body: the CRC check must catch it.
        let at = z.windows(5).position(|w| w == b"hello").unwrap();
        z[at] = b'j';
        let dir = tempfile::tempdir().unwrap();
        assert!(extract_zip(std::io::Cursor::new(z), dir.path()).is_err());
    }

    #[test]
    fn extract_sniffs_the_format() {
        let dir = tempfile::tempdir().unwrap();
        let zpath = dir.path().join("a.zip");
        std::fs::write(&zpath, zip(&[("z.txt", 0o100644, b"zip", true)])).unwrap();
        let tpath = dir.path().join("a.tgz");
        std::fs::write(&tpath, tar_gz(&[("t.txt", b'0', b"tar")])).unwrap();
        let out = dir.path().join("out");
        extract(&zpath, &out).unwrap();
        extract(&tpath, &out).unwrap();
        assert_eq!(std::fs::read(out.join("z.txt")).unwrap(), b"zip");
        assert_eq!(std::fs::read(out.join("t.txt")).unwrap(), b"tar");
    }

    /// A real vendor archive: `SWITCHYARD_TEST_ZIP=instantclient-basic-….zip cargo test
    /// -p switchyard-drivers -- --ignored real_zip`.
    #[test]
    #[ignore]
    fn real_zip() {
        let Ok(path) = std::env::var("SWITCHYARD_TEST_ZIP") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        extract(Path::new(&path), dir.path()).unwrap();
        let n = std::fs::read_dir(dir.path()).unwrap().count();
        assert!(n > 0);
    }
}
