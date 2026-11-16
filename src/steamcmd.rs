//! Runs steamcmd.exe and finds the downloaded .gma / _legacy.bin.

use crate::config::GMOD_APP_ID;
use anyhow::{bail, Context, Result};
use regex::Regex;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Downloads several items in one steamcmd run.
/// Returns the ids steamcmd reported as `Success. Downloaded item <id>`,
/// together with the file path from that line (if steamcmd printed one).
/// `install_dir` is `+force_install_dir` (private cache in the temp folder); `None` uses steamcmd's own cache.
pub fn download_items(
    exe: &Path,
    ids: &[u64],
    install_dir: Option<&Path>,
    log: &mut dyn FnMut(String),
) -> Result<HashMap<u64, Option<PathBuf>>> {
    let mut ok = HashMap::new();
    if ids.is_empty() {
        return Ok(ok);
    }
    if !exe.is_file() {
        bail!("steamcmd.exe not found: {}", exe.display());
    }

    let mut cmd = Command::new(exe);
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
    }
    if let Some(d) = install_dir {
        std::fs::create_dir_all(d).with_context(|| format!("cannot create {}", d.display()))?;
        cmd.arg("+force_install_dir").arg(d);
    }
    cmd.arg("+login").arg("anonymous");
    for id in ids {
        cmd.arg("+workshop_download_item").arg(GMOD_APP_ID.to_string()).arg(id.to_string());
    }
    cmd.arg("+quit")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    log(format!("Starting steamcmd for {} item(s)...", ids.len()));
    let mut child = cmd.spawn().with_context(|| format!("cannot start {}", exe.display()))?;

    // Read stdout and stderr on two threads, merge lines into one channel.
    let (tx, rx) = mpsc::channel::<String>();
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(spawn_line_reader(out, tx.clone()));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(spawn_line_reader(err, tx.clone()));
    }
    drop(tx);

    let success_re = Regex::new(r#"Success\. Downloaded item (\d+)(?: to "([^"]+)")?"#).unwrap();
    for line in rx {
        if let Some(c) = success_re.captures(&line) {
            if let Ok(id) = c[1].parse::<u64>() {
                ok.insert(id, c.get(2).map(|m| PathBuf::from(m.as_str())));
            }
        }
        log(format!("[steamcmd] {line}"));
    }
    for r in readers {
        let _ = r.join();
    }
    let status = child.wait().context("steamcmd failed")?;
    log(format!("steamcmd finished (exit code {}).", status.code().unwrap_or(-1)));
    Ok(ok)
}

fn spawn_line_reader<R: Read + Send + 'static>(mut src: R, tx: mpsc::Sender<String>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut line: Vec<u8> = Vec::new();
        let flush = |line: &mut Vec<u8>| {
            let s = String::from_utf8_lossy(line).trim().to_string();
            line.clear();
            if !s.is_empty() {
                let _ = tx.send(s);
            }
        };
        loop {
            match src.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    for &b in &buf[..n] {
                        if b == b'\n' || b == b'\r' {
                            flush(&mut line);
                        } else {
                            line.push(b);
                        }
                    }
                }
            }
        }
        flush(&mut line);
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum GmaSource {
    /// A regular `.gma`.
    Gma(PathBuf),
    /// Old format `*_legacy.bin` (LZMA alone).
    Legacy(PathBuf),
}

fn classify(path: &Path) -> Option<GmaSource> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    if name.ends_with(".gma") {
        Some(GmaSource::Gma(path.to_path_buf()))
    } else if name.ends_with("_legacy.bin") {
        Some(GmaSource::Legacy(path.to_path_buf()))
    } else {
        None
    }
}

/// The addon file: the one named in steamcmd's `Success` line, otherwise a search of the folder.
/// `None` means there are no files (for example, a stale steamcmd cache).
pub fn resolve_source(reported: Option<&Path>, content_dir: &Path) -> Option<GmaSource> {
    if let Some(p) = reported.filter(|p| p.is_file()) {
        if let Some(s) = classify(p) {
            return Some(s);
        }
    }
    find_gma_source(content_dir).ok()
}

/// Finds the newest addon file in `<steamcmd>\steamapps\workshop\content\4000\<id>\`.
pub fn find_gma_source(content_dir: &Path) -> Result<GmaSource> {
    let rd = std::fs::read_dir(content_dir).with_context(|| format!("no download folder {}", content_dir.display()))?;
    let mut best: Option<(std::time::SystemTime, GmaSource)> = None;
    for e in rd.flatten() {
        let path = e.path();
        if !path.is_file() {
            continue;
        }
        let Some(src) = classify(&path) else { continue };
        let mtime = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().map_or(true, |(t, _)| mtime > *t) {
            best = Some((mtime, src));
        }
    }
    match best {
        Some((_, s)) => Ok(s),
        None => bail!("no .gma or *_legacy.bin in {}", content_dir.display()),
    }
}
