//! GMA extraction (own code, no gmad.exe) and LZMA for old `_legacy.bin` files.

use anyhow::{anyhow, bail, Context, Result};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub const MAGIC: &[u8; 4] = b"GMAD";

#[derive(Debug, Clone, PartialEq)]
pub struct GmaEntry {
    pub path: String,
    pub size: u64,
    pub crc: u32,
}

#[derive(Debug, Clone, Default)]
pub struct GmaHeader {
    pub version: u8,
    pub steamid: u64,
    pub timestamp: u64,
    pub required: Vec<String>,
    pub name: String,
    pub description: String,
    pub author: String,
    pub addon_version: i32,
    pub entries: Vec<GmaEntry>,
}

#[derive(Debug, Clone, Default)]
pub struct ExtractReport {
    pub files_written: usize,
    pub bytes_written: u64,
    pub skipped: Vec<String>,
    pub crc_mismatches: Vec<String>,
}

fn read_arr<const N: usize, R: Read>(r: &mut R) -> Result<[u8; N]> {
    let mut b = [0u8; N];
    r.read_exact(&mut b).context("GMA file is truncated")?;
    Ok(b)
}
fn read_u8<R: Read>(r: &mut R) -> Result<u8> {
    Ok(read_arr::<1, R>(r)?[0])
}
fn read_u32<R: Read>(r: &mut R) -> Result<u32> {
    Ok(u32::from_le_bytes(read_arr(r)?))
}
fn read_i32<R: Read>(r: &mut R) -> Result<i32> {
    Ok(i32::from_le_bytes(read_arr(r)?))
}
fn read_u64<R: Read>(r: &mut R) -> Result<u64> {
    Ok(u64::from_le_bytes(read_arr(r)?))
}
fn read_i64<R: Read>(r: &mut R) -> Result<i64> {
    Ok(i64::from_le_bytes(read_arr(r)?))
}

/// A `\0`-terminated string.
fn read_cstr<R: Read>(r: &mut R, max: usize) -> Result<String> {
    let mut buf = Vec::new();
    loop {
        let b = read_u8(r)?;
        if b == 0 {
            break;
        }
        buf.push(b);
        if buf.len() > max {
            bail!("string too long in GMA (corrupted file?)");
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Reads the header and file table. Afterwards `r` points at the start of file data.
pub fn read_header<R: Read>(r: &mut R) -> Result<GmaHeader> {
    let magic: [u8; 4] = read_arr(r).context("file is too short to be a GMA")?;
    if &magic != MAGIC {
        bail!("not a GMA file (no GMAD signature)");
    }
    let mut h = GmaHeader { version: read_u8(r)?, ..Default::default() };
    if h.version > 3 {
        bail!("unknown GMA version: {}", h.version);
    }
    h.steamid = read_u64(r)?;
    h.timestamp = read_u64(r)?;
    if h.version > 1 {
        loop {
            let s = read_cstr(r, 4096)?;
            if s.is_empty() {
                break;
            }
            h.required.push(s);
            if h.required.len() > 10_000 {
                bail!("corrupted GMA: too many required strings");
            }
        }
    }
    h.name = read_cstr(r, 64 * 1024)?;
    h.description = read_cstr(r, 16 * 1024 * 1024)?;
    h.author = read_cstr(r, 64 * 1024)?;
    h.addon_version = read_i32(r)?;
    loop {
        let number = read_u32(r)?;
        if number == 0 {
            break;
        }
        let path = read_cstr(r, 4096)?;
        let size = read_i64(r)?;
        if size < 0 {
            bail!("corrupted GMA: negative size for {path}");
        }
        let crc = read_u32(r)?;
        h.entries.push(GmaEntry { path, size: size as u64, crc });
        if h.entries.len() > 1_000_000 {
            bail!("corrupted GMA: too many files");
        }
    }
    Ok(h)
}

const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1",
    "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Normalizes a path from a GMA: lowercase, `/` as separator.
/// `None` means the path is unsafe (`..`, absolute, `:`, forbidden characters) and the file must be skipped.
pub fn sanitize_path(raw: &str) -> Option<String> {
    let p = raw.trim().to_lowercase();
    if p.is_empty() || p.contains(':') || p.contains('\0') {
        return None;
    }
    if p.starts_with('/') || p.starts_with('\\') {
        return None;
    }
    let p = p.replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for comp in p.split('/') {
        match comp {
            "" | "." => continue,
            ".." => return None,
            c => {
                if c.chars().any(|ch| ch.is_control() || "<>\"|?*".contains(ch)) {
                    return None;
                }
                // Windows trims trailing dots/spaces, which could hide a `..`.
                if c.ends_with('.') || c.ends_with(' ') {
                    return None;
                }
                let stem = c.split('.').next().unwrap_or(c);
                if RESERVED.contains(&stem) {
                    return None;
                }
                parts.push(c);
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

/// Safe full path inside `dest` (or `None`).
pub fn safe_target(dest: &Path, raw: &str) -> Option<PathBuf> {
    let rel = sanitize_path(raw)?;
    let mut p = dest.to_path_buf();
    for c in rel.split('/') {
        p.push(c);
    }
    if p.starts_with(dest) {
        Some(p)
    } else {
        None
    }
}

/// Extracts a GMA stream into `dest`.
pub fn extract<R: Read>(mut r: R, dest: &Path, log: &mut dyn FnMut(String)) -> Result<(GmaHeader, ExtractReport)> {
    let header = read_header(&mut r)?;
    fs::create_dir_all(dest).with_context(|| format!("cannot create {}", dest.display()))?;
    let mut rep = ExtractReport::default();
    let mut buf = vec![0u8; 256 * 1024];

    for e in &header.entries {
        let mut out = match safe_target(dest, &e.path) {
            Some(target) => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
                }
                let f = File::create(&target).with_context(|| format!("cannot write {}", target.display()))?;
                Some(BufWriter::new(f))
            }
            None => {
                log(format!("Warning: skipped file with unsafe path: \"{}\"", e.path));
                rep.skipped.push(e.path.clone());
                None
            }
        };
        let mut hasher = crc32fast::Hasher::new();
        let mut left = e.size;
        while left > 0 {
            let n = left.min(buf.len() as u64) as usize;
            r.read_exact(&mut buf[..n]).with_context(|| format!("GMA is truncated at {}", e.path))?;
            hasher.update(&buf[..n]);
            if let Some(w) = out.as_mut() {
                w.write_all(&buf[..n])?;
            }
            left -= n as u64;
        }
        let crc = hasher.finalize();
        if let Some(mut w) = out {
            w.flush()?;
            rep.files_written += 1;
            rep.bytes_written += e.size;
            // Some old packers write CRC 0, meaning "not set" rather than an error.
            if e.crc != 0 && crc != e.crc {
                log(format!("Warning: CRC mismatch: {} (GMA says {:08x}, actual {:08x})", e.path, e.crc, crc));
                rep.crc_mismatches.push(e.path.clone());
            }
        }
    }
    Ok((header, rep))
}

/// Checks that the file starts with `GMAD`.
pub fn check_magic(path: &Path) -> Result<()> {
    let mut f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut m = [0u8; 4];
    f.read_exact(&mut m).context("file is too short")?;
    if &m != MAGIC {
        bail!("{} is not a GMA (no GMAD signature)", path.display());
    }
    Ok(())
}

/// LZMA "alone": 13-byte header, the size may be unknown (0xFF..FF)
/// and the end marker may be missing; neither is an error.
pub fn decompress_lzma_alone<R: BufRead, W: Write>(input: &mut R, output: &mut W) -> Result<()> {
    let opts = lzma_rs::decompress::Options {
        unpacked_size: lzma_rs::decompress::UnpackedSize::ReadFromHeader,
        memlimit: None,
        allow_incomplete: true,
    };
    lzma_rs::lzma_decompress_with_options(input, output, &opts).map_err(|e| anyhow!("LZMA error: {e:?}"))
}

/// Decompresses `*_legacy.bin` into `dst` and checks that the result is a GMA.
pub fn decompress_legacy_file(src: &Path, dst: &Path) -> Result<()> {
    let f = File::open(src).with_context(|| format!("cannot open {}", src.display()))?;
    let mut input = BufReader::new(f);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let out = File::create(dst).with_context(|| format!("cannot create {}", dst.display()))?;
    let mut output = BufWriter::new(out);
    decompress_lzma_alone(&mut input, &mut output)?;
    output.flush()?;
    drop(output);
    check_magic(dst).context("LZMA output is not a GMA")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a GMA in memory.
    pub fn build_gma(version: u8, files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(MAGIC);
        v.push(version);
        v.extend_from_slice(&76561197960287930u64.to_le_bytes());
        v.extend_from_slice(&1_700_000_000u64.to_le_bytes());
        if version > 1 {
            v.extend_from_slice(b"content\0");
            v.push(0); // end of required list
        }
        v.extend_from_slice(b"Test Addon\0");
        v.extend_from_slice(b"{\"description\":\"desc\",\"type\":\"tool\",\"tags\":[\"fun\"]}\0");
        v.extend_from_slice(b"Author\0");
        v.extend_from_slice(&1i32.to_le_bytes());
        for (i, (path, data)) in files.iter().enumerate() {
            v.extend_from_slice(&(i as u32 + 1).to_le_bytes());
            v.extend_from_slice(path.as_bytes());
            v.push(0);
            v.extend_from_slice(&(data.len() as i64).to_le_bytes());
            v.extend_from_slice(&crc32fast::hash(data).to_le_bytes());
        }
        v.extend_from_slice(&0u32.to_le_bytes());
        for (_, data) in files {
            v.extend_from_slice(data);
        }
        let crc = crc32fast::hash(&v);
        v.extend_from_slice(&crc.to_le_bytes());
        v
    }

    pub fn temp_test_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir()
            .join("workshop_drop_tests")
            .join(format!("{name}_{}_{nanos}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn extracts_v3_with_two_files() {
        let lua: &[u8] = b"print('hello')\n";
        let vmt: &[u8] = b"\"VertexLitGeneric\" {}";
        let gma = build_gma(3, &[("lua/autorun/test.lua", lua), ("Materials/Test/X.vmt", vmt)]);
        let dir = temp_test_dir("gma_ok");
        let mut logs = Vec::new();
        let (h, rep) = extract(&gma[..], &dir, &mut |m| logs.push(m)).unwrap();

        assert_eq!(h.version, 3);
        assert_eq!(h.name, "Test Addon");
        assert_eq!(h.author, "Author");
        assert_eq!(h.required, vec!["content".to_string()]);
        assert_eq!(h.entries.len(), 2);
        assert_eq!(rep.files_written, 2);
        assert_eq!(rep.bytes_written, (lua.len() + vmt.len()) as u64);
        assert!(rep.skipped.is_empty());
        assert!(rep.crc_mismatches.is_empty(), "{logs:?}");
        assert_eq!(fs::read(dir.join("lua").join("autorun").join("test.lua")).unwrap(), lua);
        // path was lowercased
        assert_eq!(fs::read(dir.join("materials").join("test").join("x.vmt")).unwrap(), vmt);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn extracts_v1_without_required_list() {
        let gma = build_gma(1, &[("lua/a.lua", b"x")]);
        let dir = temp_test_dir("gma_v1");
        let (h, rep) = extract(&gma[..], &dir, &mut |_| {}).unwrap();
        assert!(h.required.is_empty());
        assert_eq!(rep.files_written, 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_parent_dir_paths() {
        let root = temp_test_dir("gma_evil");
        let dest = root.join("addon");
        let gma = build_gma(
            3,
            &[
                ("../evil.txt", b"bad"),
                ("lua/../../evil2.txt", b"bad"),
                ("..\\evil3.txt", b"bad"),
                ("c:/windows/evil4.txt", b"bad"),
                ("/abs/evil5.txt", b"bad"),
                ("lua/ok.lua", b"good"),
            ],
        );
        let mut logs = Vec::new();
        let (_, rep) = extract(&gma[..], &dest, &mut |m| logs.push(m)).unwrap();
        assert_eq!(rep.files_written, 1);
        assert_eq!(rep.skipped.len(), 5);
        assert_eq!(logs.len(), 5);
        assert!(!root.join("evil.txt").exists());
        assert!(!root.join("evil2.txt").exists());
        assert!(!root.join("evil3.txt").exists());
        assert_eq!(fs::read(dest.join("lua").join("ok.lua")).unwrap(), b"good");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn sanitize_rules() {
        assert_eq!(sanitize_path("Lua/Weapons/X.lua").as_deref(), Some("lua/weapons/x.lua"));
        assert_eq!(sanitize_path("lua\\autorun\\a.lua").as_deref(), Some("lua/autorun/a.lua"));
        assert_eq!(sanitize_path("lua/./a.lua").as_deref(), Some("lua/a.lua"));
        assert_eq!(sanitize_path(".."), None);
        assert_eq!(sanitize_path("a/../../b"), None);
        assert_eq!(sanitize_path("C:/x"), None);
        assert_eq!(sanitize_path("\\x"), None);
        assert_eq!(sanitize_path("lua/con.txt"), None);
        assert_eq!(sanitize_path("lua/a.../b"), None);
        assert_eq!(sanitize_path(""), None);
    }

    #[test]
    fn crc_mismatch_is_warning_not_error() {
        let mut gma = build_gma(3, &[("lua/a.lua", b"abc")]);
        // corrupt the file data (it sits right before the trailing CRC)
        let n = gma.len();
        gma[n - 5] ^= 0xFF;
        let dir = temp_test_dir("gma_crc");
        let mut logs = Vec::new();
        let (_, rep) = extract(&gma[..], &dir, &mut |m| logs.push(m)).unwrap();
        assert_eq!(rep.files_written, 1);
        assert_eq!(rep.crc_mismatches, vec!["lua/a.lua".to_string()]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn not_a_gma() {
        let dir = temp_test_dir("gma_bad");
        assert!(extract(&b"NOPE1234"[..], &dir, &mut |_| {}).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lzma_alone_without_end_marker_and_unknown_size() {
        let gma = build_gma(3, &[("lua/autorun/big.lua", "-- test\n".repeat(5000).as_bytes())]);

        // Known size in the header -> lzma-rs does not write an end marker.
        let mut packed = Vec::new();
        let opts = lzma_rs::compress::Options {
            unpacked_size: lzma_rs::compress::UnpackedSize::WriteToHeader(Some(gma.len() as u64)),
        };
        lzma_rs::lzma_compress_with_options(&mut &gma[..], &mut packed, &opts).unwrap();
        // Make the size "unknown", like old _legacy.bin files.
        for b in &mut packed[5..13] {
            *b = 0xFF;
        }

        let mut out = Vec::new();
        decompress_lzma_alone(&mut &packed[..], &mut out).unwrap();
        assert_eq!(&out[..4], MAGIC);
        assert_eq!(out, gma);
    }

    #[test]
    fn lzma_alone_with_end_marker() {
        let data = b"GMAD hello hello hello".repeat(100);
        let mut packed = Vec::new();
        lzma_rs::lzma_compress(&mut &data[..], &mut packed).unwrap();
        let mut out = Vec::new();
        decompress_lzma_alone(&mut &packed[..], &mut out).unwrap();
        assert_eq!(out, data);
    }
}
