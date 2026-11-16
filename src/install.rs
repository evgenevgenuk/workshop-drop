//! Link analysis, installing into addons, the loader addon, installed list.
//! Everything here runs on a background thread and reports through `Reporter`.

use crate::config::{Config, GMOD_APP_ID};
use crate::gma;
use crate::steam_api::{self, ItemDetails, RequiredItems, SteamApi};
use crate::steamcmd::{self, GmaSource};
use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::Arc;

// ---------------------------------------------------------------- events for the UI

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Info,
    Download,
    Extract,
    Done,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Origin {
    Direct,
    Collection(String),
    Dependency(String),
}

#[derive(Debug, Clone)]
pub struct PlanItem {
    pub id: u64,
    pub title: String,
    pub size: u64,
    pub origin: Origin,
    pub selected: bool,
    /// Existing `ws_<id>_*` folders.
    pub existing: Vec<String>,
    /// A manually placed folder: `something_<id>` without the `ws_` prefix.
    pub misnamed: Option<String>,
    /// Rename `misnamed` instead of downloading.
    pub rename_instead: bool,
    pub is_map: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub items: Vec<PlanItem>,
    pub warnings: Vec<String>,
    pub needs_confirmation: bool,
}

#[derive(Debug, Clone)]
pub struct InstalledAddon {
    pub dir_name: String,
    pub id: u64,
    pub title: String,
    pub size: u64,
    pub files: usize,
    pub modified: String,
}

pub enum Event {
    Log(String),
    Progress { id: u64, title: String, stage: Stage, note: String },
    Plan(Plan),
    Installed(Vec<InstalledAddon>),
    Finished,
}

#[derive(Clone)]
pub struct Reporter {
    tx: Sender<Event>,
    repaint: Arc<dyn Fn() + Send + Sync>,
}

impl Reporter {
    pub fn new(tx: Sender<Event>, repaint: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self { tx, repaint }
    }
    pub fn send(&self, e: Event) {
        let _ = self.tx.send(e);
        (self.repaint)();
    }
    pub fn log(&self, s: impl Into<String>) {
        self.send(Event::Log(s.into()));
    }
    pub fn progress(&self, id: u64, title: &str, stage: Stage, note: impl Into<String>) {
        self.send(Event::Progress { id, title: title.to_string(), stage, note: note.into() });
    }
}

// ---------------------------------------------------------------- helpers

/// Title -> `[a-z0-9_]`, no `_` at the ends, at most 32 chars, empty -> `addon`.
/// Leading tags like "[Official] Precision Tool" are dropped if something is left after them.
pub fn slugify(title: &str) -> String {
    let mut t = title.trim();
    while t.starts_with('[') {
        match t.find(']') {
            Some(end) if t[end + 1..].chars().any(|c| c.is_ascii_alphanumeric()) => t = t[end + 1..].trim_start(),
            _ => break,
        }
    }
    let mut s = String::new();
    let mut last_us = true;
    for ch in t.chars().flat_map(|c| c.to_lowercase()) {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            s.push(ch);
            last_us = false;
        } else if !last_us {
            s.push('_');
            last_us = true;
        }
    }
    let mut t: String = s.trim_matches('_').chars().take(32).collect();
    while t.ends_with('_') {
        t.pop();
    }
    if t.is_empty() {
        "addon".to_string()
    } else {
        t
    }
}

pub fn fmt_size(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b < K {
        format!("{bytes} B")
    } else if b < K * K {
        format!("{:.1} KB", b / K)
    } else if b < K * K * K {
        format!("{:.1} MB", b / K / K)
    } else {
        format!("{:.2} GB", b / K / K / K)
    }
}

fn list_dirs(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// `ws_<id>_*` folders (and `ws_<id>`).
pub fn find_existing(addons: &Path, id: u64) -> Vec<String> {
    let prefix = format!("ws_{id}_");
    let exact = format!("ws_{id}");
    list_dirs(addons)
        .into_iter()
        .filter(|n| {
            let l = n.to_lowercase();
            l.starts_with(&prefix) || l == exact
        })
        .collect()
}

/// A folder without `ws_` that ends with `_<id>` (or is named just `<id>`).
pub fn find_misnamed(addons: &Path, id: u64) -> Option<String> {
    let suffix = format!("_{id}");
    let plain = id.to_string();
    list_dirs(addons).into_iter().find(|n| {
        let l = n.to_lowercase();
        !l.starts_with("ws_") && (l.ends_with(&suffix) || l == plain)
    })
}

/// File count and total size of a folder.
pub fn dir_stats(dir: &Path) -> (usize, u64) {
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                files += 1;
                bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    (files, bytes)
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src)? {
        let e = e?;
        let to = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir_all(&e.path(), &to)?;
        } else {
            fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
}

/// Moves a folder; if `rename` fails (another drive) it copies and removes the source.
fn move_dir(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        bail!("{} already exists", dst.display());
    }
    if fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    copy_dir_all(src, dst).with_context(|| format!("cannot copy {} -> {}", src.display(), dst.display()))?;
    fs::remove_dir_all(src).with_context(|| format!("copied, but cannot remove {}", src.display()))?;
    Ok(())
}

/// Deletes `addons\<dir_name>`. Only folders directly inside addons are accepted.
pub fn delete_addon_dir(cfg: &Config, dir_name: &str) -> Result<()> {
    if dir_name.is_empty() || dir_name.contains(['/', '\\', ':']) || dir_name == "." || dir_name == ".." {
        bail!("refusing to delete suspicious folder name \"{dir_name}\"");
    }
    let dir = cfg.addons_dir.join(dir_name);
    if !dir.is_dir() {
        bail!("folder {} does not exist", dir.display());
    }
    fs::remove_dir_all(&dir).with_context(|| format!("cannot delete {}", dir.display()))
}

fn has_bsp(dir: &Path) -> bool {
    fs::read_dir(dir.join("maps"))
        .map(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().to_lowercase().ends_with(".bsp")))
        .unwrap_or(false)
}

fn looks_empty(dir: &Path) -> bool {
    !dir.join("lua").is_dir() && !["models", "materials", "sound"].iter().any(|d| dir.join(d).is_dir())
}

/// Is the server (srcds_win64.exe) running?
pub fn srcds_running() -> bool {
    use sysinfo::{ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    let running = sys.processes().values().any(|p| {
        let n = p.name();
        n.eq_ignore_ascii_case("srcds_win64.exe") || n.eq_ignore_ascii_case("srcds.exe")
    });
    running
}

fn join_ids(ids: &[u64]) -> String {
    ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
}

// ---------------------------------------------------------------- loader addon

pub const LOADER_DIR: &str = "workshop_drop_loader";

const LOADER_LUA: &str = r#"-- Created by WorkshopDrop (https://github.com/evgenevgenuk/workshop-drop) - safe to edit or delete.
-- Makes joining players download every addons/ws_<id>_* folder from the Steam Workshop.
if not SERVER then return end

hook.Add("Initialize", "WorkshopDrop_AddWorkshop", function()
	local _, folders = file.Find("addons/*", "MOD")
	local count = 0
	for _, name in ipairs(folders or {}) do
		local id = string.match(name, "^ws_(%d+)")
		if id then
			resource.AddWorkshop(id)
			count = count + 1
		end
	end
	print("[WorkshopDrop] resource.AddWorkshop for " .. count .. " addon(s)")
end)
"#;

/// Creates `addons\workshop_drop_loader` if it is missing. Never overwrites an existing one.
pub fn ensure_loader(cfg: &Config, r: &Reporter) {
    if !cfg.create_loader {
        return;
    }
    let root = cfg.addons_dir.join(LOADER_DIR);
    let lua = root.join("lua").join("autorun").join("server").join("workshop_drop_loader.lua");
    if lua.exists() {
        return;
    }
    let res = (|| -> Result<()> {
        fs::create_dir_all(lua.parent().unwrap())?;
        fs::write(&lua, LOADER_LUA)?;
        fs::write(
            root.join("addon.json"),
            "{\n  \"title\": \"WorkshopDrop loader\",\n  \"type\": \"servercontent\",\n  \"tags\": []\n}\n",
        )?;
        Ok(())
    })();
    match res {
        Ok(()) => r.log(format!(
            "Created addons\\{LOADER_DIR}: it calls resource.AddWorkshop for every ws_<id>_* folder so players download them on join."
        )),
        Err(e) => r.log(format!("Warning: cannot create the loader addon: {e:#}")),
    }
}

// ---------------------------------------------------------------- analysis

fn valid_item(d: &ItemDetails, r: &Reporter, show_row: bool) -> bool {
    let title = if d.title.is_empty() { d.id.to_string() } else { d.title.clone() };
    let err = if d.result != 1 {
        Some("item is hidden, deleted or unavailable")
    } else if d.consumer_app_id != GMOD_APP_ID {
        Some("this is not a Garry's Mod addon")
    } else {
        None
    };
    if let Some(e) = err {
        r.log(format!("Error: {} ({}): {e}.", title, d.id));
        if show_row {
            r.progress(d.id, &title, Stage::Error, e);
        }
        return false;
    }
    true
}

fn fetch_pages(api: &SteamApi, ids: &[u64]) -> Vec<(u64, Result<String>)> {
    let mut out = Vec::new();
    for (n, chunk) in ids.chunks(3).enumerate() {
        if n > 0 {
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
        let res: Vec<(u64, Result<String>)> = std::thread::scope(|s| {
            let handles: Vec<_> = chunk.iter().map(|&id| s.spawn(move || (id, api.fetch_page(id)))).collect();
            handles
                .into_iter()
                .zip(chunk.iter())
                .map(|(h, &id)| h.join().unwrap_or_else(|_| (id, Err(anyhow!("thread panicked")))))
                .collect()
        });
        out.extend(res);
    }
    out
}

/// Parses the links, asks Steam, expands collections and finds dependencies.
/// Sends `Event::Plan`, or `Event::Finished` if there is nothing to install.
pub fn analyze(cfg: &Config, input: &str, force_update: bool, r: &Reporter) {
    match analyze_inner(cfg, input, force_update, r) {
        Ok(Some(plan)) => r.send(Event::Plan(plan)),
        Ok(None) => r.send(Event::Finished),
        Err(e) => {
            r.log(format!("Error: {e:#}"));
            r.send(Event::Finished);
        }
    }
}

fn analyze_inner(cfg: &Config, input: &str, force_update: bool, r: &Reporter) -> Result<Option<Plan>> {
    let mut ids: Vec<u64> = Vec::new();
    for tok in input.split_whitespace() {
        match steam_api::parse_workshop_id(tok) {
            Some(id) => {
                if !ids.contains(&id) {
                    ids.push(id)
                }
            }
            None => r.log(format!(
                "Error: no Workshop item id in \"{tok}\". Paste a link like \
                 https://steamcommunity.com/sharedfiles/filedetails/?id=123 or just the number."
            )),
        }
    }
    if ids.is_empty() {
        r.log("Nothing to install.");
        return Ok(None);
    }
    for &id in &ids {
        r.progress(id, &id.to_string(), Stage::Info, "");
    }

    r.log(format!("Asking Steam about {} item(s)...", ids.len()));
    let api = SteamApi::new()?;
    let details = match api.get_details(&ids) {
        Ok(d) => d,
        Err(e) => {
            for &id in &ids {
                r.progress(id, &id.to_string(), Stage::Error, "Steam API unavailable");
            }
            return Err(e.context("cannot get item info from Steam"));
        }
    };
    let by_id: HashMap<u64, ItemDetails> = details.into_iter().map(|d| (d.id, d)).collect();

    let ok_ids: Vec<u64> = ids
        .iter()
        .copied()
        .filter(|id| by_id.get(id).map_or(false, |d| d.result == 1))
        .collect();
    let collections = if ok_ids.is_empty() {
        HashMap::new()
    } else {
        api.get_collection_children(&ok_ids).unwrap_or_else(|e| {
            r.log(format!("Warning: cannot check whether this is a collection: {e:#}"));
            HashMap::new()
        })
    };

    let mut plan = Plan::default();
    let mut in_plan: HashSet<u64> = HashSet::new();
    let mut collection_children: Vec<(u64, String)> = Vec::new(); // (child, collection title)

    for &id in &ids {
        let Some(d) = by_id.get(&id) else {
            r.log(format!("Error: Steam knows nothing about item {id}."));
            r.progress(id, &id.to_string(), Stage::Error, "item is hidden, deleted or unavailable");
            continue;
        };
        if d.result == 1 {
            if let Some(children) = collections.get(&id) {
                r.log(format!("\"{}\" is a collection ({} entries). Offering to install all of them.", d.title, children.len()));
                r.progress(id, &d.title, Stage::Done, format!("collection -> {} items", children.len()));
                let mut nested: Vec<u64> = Vec::new();
                for &(cid, ft) in children {
                    if ft == 2 {
                        nested.push(cid);
                    } else {
                        collection_children.push((cid, d.title.clone()));
                    }
                }
                // Nested collections (up to 3 levels).
                let mut depth = 0;
                let mut seen_coll: HashSet<u64> = HashSet::from([id]);
                while !nested.is_empty() && depth < 3 {
                    nested.retain(|c| seen_coll.insert(*c));
                    let sub = api.get_collection_children(&nested).unwrap_or_default();
                    nested.clear();
                    for (_, ch) in sub {
                        for (cid, ft) in ch {
                            if ft == 2 {
                                nested.push(cid);
                            } else {
                                collection_children.push((cid, d.title.clone()));
                            }
                        }
                    }
                    depth += 1;
                }
                continue;
            }
        }
        if !valid_item(d, r, true) {
            continue;
        }
        r.progress(id, &d.title, Stage::Info, "");
        if in_plan.insert(id) {
            plan.items.push(new_item(cfg, d, Origin::Direct));
        }
    }

    // Collection items.
    let child_ids: Vec<u64> = {
        let mut seen = HashSet::new();
        collection_children
            .iter()
            .map(|(c, _)| *c)
            .filter(|c| !in_plan.contains(c) && seen.insert(*c))
            .collect()
    };
    if !child_ids.is_empty() {
        let cd = api.get_details(&child_ids).context("cannot get collection items")?;
        let cd: HashMap<u64, ItemDetails> = cd.into_iter().map(|d| (d.id, d)).collect();
        for (cid, ctitle) in &collection_children {
            if in_plan.contains(cid) {
                continue;
            }
            let Some(d) = cd.get(cid) else { continue };
            if !valid_item(d, r, false) {
                continue;
            }
            in_plan.insert(*cid);
            plan.items.push(new_item(cfg, d, Origin::Collection(ctitle.clone())));
        }
    }

    // Dependencies (Required items), up to 3 levels.
    let mut queue: Vec<u64> = plan.items.iter().map(|i| i.id).collect();
    let mut scanned: HashSet<u64> = HashSet::new();
    for _ in 0..3 {
        let to_scan: Vec<u64> = queue.drain(..).filter(|id| scanned.insert(*id)).collect();
        if to_scan.is_empty() {
            break;
        }
        r.log(format!("Checking dependencies ({} Workshop page(s))...", to_scan.len()));
        let mut new_deps: Vec<(u64, String)> = Vec::new(); // (dep, parent title)
        let mut failed: Vec<String> = Vec::new();
        let mut last_err = String::new();
        for (id, res) in fetch_pages(&api, &to_scan) {
            let parent = plan.items.iter().find(|i| i.id == id).map(|i| i.title.clone()).unwrap_or_default();
            match res {
                Err(e) => {
                    failed.push(parent);
                    last_err = format!("{e:#}");
                }
                Ok(html) => match steam_api::parse_required_items(&html, id) {
                    RequiredItems::None => {}
                    RequiredItems::Unparsed => {
                        let msg = format!("Warning: \"{parent}\" has a Required items block that could not be parsed.");
                        r.log(&msg);
                        plan.warnings.push(msg);
                    }
                    RequiredItems::Found(list) => {
                        let names: Vec<String> = list
                            .iter()
                            .map(|(i, n)| if n.is_empty() { i.to_string() } else { format!("{n} ({i})") })
                            .collect();
                        r.log(format!("\"{parent}\" requires: {}", names.join(", ")));
                        for (dep, _) in list {
                            if !in_plan.contains(&dep) && !new_deps.iter().any(|(d, _)| *d == dep) {
                                new_deps.push((dep, parent.clone()));
                            }
                        }
                    }
                },
            }
        }
        if !failed.is_empty() {
            let msg = format!(
                "Warning: could not load the Workshop page for {} item(s) ({}), their dependencies were not checked. Reason: {last_err}",
                failed.len(),
                failed.join(", ")
            );
            r.log(&msg);
            plan.warnings.push(msg);
        }
        if new_deps.is_empty() {
            break;
        }
        let dep_ids: Vec<u64> = new_deps.iter().map(|(d, _)| *d).collect();
        match api.get_details(&dep_ids) {
            Err(e) => {
                let msg = format!("Warning: cannot get info about dependencies: {e:#}");
                r.log(&msg);
                plan.warnings.push(msg);
            }
            Ok(dd) => {
                let dd: HashMap<u64, ItemDetails> = dd.into_iter().map(|d| (d.id, d)).collect();
                for (dep, parent) in new_deps {
                    let Some(d) = dd.get(&dep) else { continue };
                    if !valid_item(d, r, false) {
                        continue;
                    }
                    in_plan.insert(dep);
                    plan.items.push(new_item(cfg, d, Origin::Dependency(parent)));
                    queue.push(dep);
                }
            }
        }
    }

    if plan.items.is_empty() {
        r.log("Nothing to install.");
        return Ok(None);
    }

    for it in &plan.items {
        if !it.existing.is_empty() {
            r.log(format!("\"{}\" is already installed: {}", it.title, it.existing.join(", ")));
        }
        if let Some(m) = &it.misnamed {
            r.log(format!("\"{}\": found folder \"{m}\" without the ws_ prefix, offering to rename it.", it.title));
        }
        if it.is_map {
            r.log(format!(
                "Warning: \"{}\" is a map. Maps are better added to your Steam Workshop collection, not to server content.",
                it.title
            ));
        }
    }

    plan.needs_confirmation = plan.items.iter().any(|i| {
        i.origin != Origin::Direct || i.misnamed.is_some() || i.is_map || (!force_update && !i.existing.is_empty())
    }) || !plan.warnings.is_empty();
    Ok(Some(plan))
}

fn new_item(cfg: &Config, d: &ItemDetails, origin: Origin) -> PlanItem {
    let existing = find_existing(&cfg.addons_dir, d.id);
    let misnamed = find_misnamed(&cfg.addons_dir, d.id);
    let is_map = d.is_map();
    let selected = !is_map
        && match origin {
            Origin::Direct => true,
            // Already installed dependencies / collection items are not re-downloaded by default.
            _ => existing.is_empty(),
        };
    PlanItem {
        id: d.id,
        title: if d.title.is_empty() { d.id.to_string() } else { d.title.clone() },
        size: d.file_size,
        origin,
        selected,
        rename_instead: misnamed.is_some() && existing.is_empty(),
        existing,
        misnamed,
        is_map,
    }
}

// ---------------------------------------------------------------- install

/// Installs the selected items. Sends `Event::Finished` at the end.
pub fn run_install(cfg: &Config, items: Vec<PlanItem>, r: &Reporter) {
    if let Err(e) = run_install_inner(cfg, items, r) {
        r.log(format!("Error: {e:#}"));
    }
    r.send(Event::Finished);
}

fn run_install_inner(cfg: &Config, items: Vec<PlanItem>, r: &Reporter) -> Result<()> {
    let temp = Config::temp_dir();
    fs::create_dir_all(&temp).with_context(|| format!("cannot create {}", temp.display()))?;
    let mut installed_any = false;
    let mut to_download: Vec<PlanItem> = Vec::new();

    for it in items.into_iter().filter(|i| i.selected) {
        if let (true, Some(m), true) = (it.rename_instead, it.misnamed.as_ref(), it.existing.is_empty()) {
            let new_name = format!("ws_{}_{}", it.id, slugify(&it.title));
            let src = cfg.addons_dir.join(m);
            let dst = cfg.addons_dir.join(&new_name);
            match move_dir(&src, &dst) {
                Ok(()) => {
                    r.log(format!("Renamed: {m} -> {new_name}"));
                    r.progress(it.id, &it.title, Stage::Done, format!("renamed to {new_name}"));
                    installed_any = true;
                }
                Err(e) => {
                    r.log(format!("Error renaming {m}: {e:#}"));
                    r.progress(it.id, &it.title, Stage::Error, "rename failed");
                }
            }
            continue;
        }
        if let Some(m) = &it.misnamed {
            r.log(format!("Warning: folder \"{m}\" stays next to the new ws_{}_... folder, content may be duplicated.", it.id));
        }
        to_download.push(it);
    }

    if !to_download.is_empty() {
        for it in &to_download {
            r.progress(it.id, &it.title, Stage::Download, "");
        }
        let exe = cfg.steamcmd_exe();
        let ids: Vec<u64> = to_download.iter().map(|i| i.id).collect();
        let mut log = |s: String| r.log(s);
        let mut ok = steamcmd::download_items(&exe, &ids, None, &mut log)?;
        let missing: Vec<u64> = ids.iter().copied().filter(|i| !ok.contains_key(i)).collect();
        if !missing.is_empty() {
            r.log(format!("Retrying: {}", join_ids(&missing)));
            match steamcmd::download_items(&exe, &missing, None, &mut log) {
                Ok(more) => ok.extend(more),
                Err(e) => r.log(format!("Error: {e:#}")),
            }
        }

        // Where are the files? If steamcmd says "Success" but there are no files, its cache is stale
        // (manifest says installed, folder was removed). Then download again into our own temp folder.
        let mut sources: HashMap<u64, GmaSource> = HashMap::new();
        let mut stale: Vec<u64> = Vec::new();
        for (&id, reported) in &ok {
            match steamcmd::resolve_source(reported.as_deref(), &cfg.workshop_content_dir(id)) {
                Some(s) => {
                    sources.insert(id, s);
                }
                None => stale.push(id),
            }
        }
        let dl_dir = temp.join("steamcmd_dl");
        if !stale.is_empty() {
            r.log(format!(
                "Warning: steamcmd thinks {} is already downloaded, but its cache has no files. Downloading again into a temp folder...",
                join_ids(&stale)
            ));
            match steamcmd::download_items(&exe, &stale, Some(&dl_dir), &mut log) {
                Ok(more) => {
                    for id in stale {
                        let dir = dl_dir
                            .join("steamapps")
                            .join("workshop")
                            .join("content")
                            .join(GMOD_APP_ID.to_string())
                            .join(id.to_string());
                        let reported = more.get(&id).cloned().flatten();
                        if let Some(s) = steamcmd::resolve_source(reported.as_deref(), &dir) {
                            sources.insert(id, s);
                        }
                    }
                }
                Err(e) => r.log(format!("Error: {e:#}")),
            }
        }

        for it in &to_download {
            let Some(src) = sources.get(&it.id).cloned() else {
                let why = if ok.contains_key(&it.id) { "no files after download" } else { "steamcmd did not download it" };
                r.log(format!("Error: \"{}\" ({}): {why}.", it.title, it.id));
                r.progress(it.id, &it.title, Stage::Error, why);
                continue;
            };
            r.progress(it.id, &it.title, Stage::Extract, "");
            match install_one(cfg, it, src, &temp, r) {
                Ok(summary) => {
                    installed_any = true;
                    r.progress(it.id, &it.title, Stage::Done, summary);
                }
                Err(e) => {
                    r.log(format!("Error \"{}\": {e:#}", it.title));
                    r.progress(it.id, &it.title, Stage::Error, format!("{e:#}"));
                }
            }
        }
        // Our private download copy (not steamcmd's cache) is no longer needed.
        if dl_dir.exists() {
            fs::remove_dir_all(&dl_dir).ok();
        }
    }

    if installed_any {
        ensure_loader(cfg, r);
        if srcds_running() {
            r.log("Done. Restart the server so it picks up the addon.");
        } else {
            r.log("Done. The server is not running, the addon will load on next start.");
        }
    }
    Ok(())
}

fn install_one(cfg: &Config, it: &PlanItem, src: GmaSource, temp: &Path, r: &Reporter) -> Result<String> {
    let mut temp_gma: Option<PathBuf> = None;
    let gma_path = match src {
        GmaSource::Gma(p) => p,
        GmaSource::Legacy(p) => {
            r.log(format!("\"{}\": old _legacy.bin format, decompressing LZMA...", it.title));
            let out = temp.join(format!("{}.gma", it.id));
            gma::decompress_legacy_file(&p, &out)?;
            temp_gma = Some(out.clone());
            out
        }
    };
    gma::check_magic(&gma_path)?;

    let stage_dir = temp.join(format!("stage_{}_{}", it.id, chrono::Local::now().format("%H%M%S%f")));
    if stage_dir.exists() {
        fs::remove_dir_all(&stage_dir).ok();
    }
    let file = fs::File::open(&gma_path).with_context(|| format!("cannot open {}", gma_path.display()))?;
    let res = gma::extract(BufReader::new(file), &stage_dir, &mut |m| r.log(m));
    let (header, report) = match res {
        Ok(v) => v,
        Err(e) => {
            fs::remove_dir_all(&stage_dir).ok();
            return Err(e.context("cannot extract GMA"));
        }
    };

    let title = if it.title.is_empty() || it.title == it.id.to_string() { header.name.clone() } else { it.title.clone() };
    let dir_name = format!("ws_{}_{}", it.id, slugify(&title));

    // addon.json so the Installed tab shows a title (unless the addon ships its own).
    let addon_json = stage_dir.join("addon.json");
    if !addon_json.exists() {
        let desc: serde_json::Value = serde_json::from_str(&header.description).unwrap_or(serde_json::Value::Null);
        let json = serde_json::json!({
            "title": title,
            "type": desc.get("type").cloned().unwrap_or(serde_json::Value::String(String::new())),
            "tags": desc.get("tags").cloned().unwrap_or(serde_json::json!([])),
            "workshopid": it.id,
        });
        fs::write(&addon_json, serde_json::to_string_pretty(&json)?).ok();
    }

    let is_map = has_bsp(&stage_dir);
    let empty = looks_empty(&stage_dir);

    // Update: the new version is fully extracted, now replace the old folder(s).
    for old in find_existing(&cfg.addons_dir, it.id) {
        if let Err(e) = delete_addon_dir(cfg, &old) {
            fs::remove_dir_all(&stage_dir).ok();
            return Err(e.context(format!("cannot remove the old version {old} (is a file open?)")));
        }
        r.log(format!("Removed old version \"{old}\"."));
    }

    let target = cfg.addons_dir.join(&dir_name);
    if let Err(e) = move_dir(&stage_dir, &target) {
        fs::remove_dir_all(&stage_dir).ok();
        return Err(e.context(format!("cannot place the addon into {}", target.display())));
    }
    if let Some(t) = temp_gma {
        fs::remove_file(t).ok();
    }

    let summary = format!("{dir_name}: {} files, {}", report.files_written, fmt_size(report.bytes_written));
    r.log(format!("Installed {summary}"));
    if is_map {
        r.log(format!(
            "Warning: \"{title}\" is a map. Maps are better added to your Steam Workshop collection, not to server content."
        ));
    }
    if empty {
        r.log(format!("Warning: \"{title}\" looks empty (no lua, models, materials or sound)."));
    }
    if !report.skipped.is_empty() {
        r.log(format!("Warning: \"{title}\": skipped {} file(s) with unsafe paths", report.skipped.len()));
    }
    if !report.crc_mismatches.is_empty() {
        r.log(format!("Warning: \"{title}\": CRC mismatch in {} file(s)", report.crc_mismatches.len()));
    }
    Ok(summary)
}

// ---------------------------------------------------------------- installed list

fn read_addon_title(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join("addon.json")).ok()?;
    let re = Regex::new(r#""title"\s*:\s*"((?:[^"\\]|\\.)*)""#).ok()?;
    let raw = re.captures(&text)?.get(1)?.as_str().to_string();
    let t = serde_json::from_str::<String>(&format!("\"{raw}\"")).unwrap_or(raw);
    let t = t.trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// All `ws_*` folders in addons. Titles come from addon.json, otherwise from the API.
pub fn scan_installed(cfg: &Config, r: &Reporter) {
    let mut list: Vec<InstalledAddon> = Vec::new();
    for name in list_dirs(&cfg.addons_dir) {
        let lower = name.to_lowercase();
        if !lower.starts_with("ws_") {
            continue;
        }
        let Some(id) = lower[3..].split('_').next().and_then(|s| s.parse::<u64>().ok()) else { continue };
        let dir = cfg.addons_dir.join(&name);
        let (files, size) = dir_stats(&dir);
        let modified = fs::metadata(&dir)
            .and_then(|m| m.modified())
            .map(|t| chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default();
        list.push(InstalledAddon { title: read_addon_title(&dir).unwrap_or_default(), dir_name: name, id, size, files, modified });
    }
    let missing: Vec<u64> = list.iter().filter(|a| a.title.is_empty()).map(|a| a.id).collect();
    if !missing.is_empty() {
        if let Ok(api) = SteamApi::new() {
            match api.get_details(&missing) {
                Ok(dd) => {
                    let m: HashMap<u64, String> =
                        dd.into_iter().filter(|d| !d.title.is_empty()).map(|d| (d.id, d.title)).collect();
                    for a in list.iter_mut().filter(|a| a.title.is_empty()) {
                        if let Some(t) = m.get(&a.id) {
                            a.title = t.clone();
                        }
                    }
                }
                Err(e) => r.log(format!("Warning: cannot get titles from Steam: {e:#}")),
            }
        }
        for a in list.iter_mut().filter(|a| a.title.is_empty()) {
            a.title = a.dir_name.splitn(3, '_').nth(2).unwrap_or("").replace('_', " ");
        }
    }
    r.send(Event::Installed(list));
}

/// "Remove": permanently deletes the addon folder.
pub fn remove_addon(cfg: &Config, dir_name: &str, r: &Reporter) {
    match delete_addon_dir(cfg, dir_name) {
        Ok(()) => r.log(format!("Removed \"{dir_name}\".")),
        Err(e) => r.log(format!("Error: cannot remove {dir_name}: {e:#}")),
    }
    if srcds_running() {
        r.log("Restart the server to apply the change.");
    }
    r.send(Event::Finished);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg(root: &Path) -> Config {
        Config {
            server_dir: root.to_path_buf(),
            addons_dir: root.join("addons"),
            steamcmd_dir: root.join("steamcmd"),
            create_loader: true,
        }
    }

    #[test]
    fn slug_basic() {
        assert_eq!(slugify("Precision Tool"), "precision_tool");
        assert_eq!(slugify("M9K Assault Rifles"), "m9k_assault_rifles");
        assert_eq!(slugify("3D2D Textscreens"), "3d2d_textscreens");
        assert_eq!(slugify("Multi-Parent/Unparent Tool"), "multi_parent_unparent_tool");
    }

    #[test]
    fn slug_non_latin() {
        assert_eq!(slugify("Зброя"), "addon");
        assert_eq!(slugify("Мій аддон v2"), "v2");
        assert_eq!(slugify("Зброя M9K — пак"), "m9k");
        assert_eq!(slugify("武器 pack"), "pack");
    }

    #[test]
    fn slug_spaces_and_specials() {
        assert_eq!(slugify("   Hello,   World!!!  "), "hello_world");
        assert_eq!(slugify("[TFA] Base__Weapon (v2.0)"), "base_weapon_v2_0");
        assert_eq!(slugify("[Official] Precision Tool"), "precision_tool");
        assert_eq!(slugify("[Official]"), "official");
        assert_eq!(slugify("[A][B] Pack"), "pack");
        assert_eq!(slugify("___"), "addon");
        assert_eq!(slugify(""), "addon");
        assert_eq!(slugify("★★★"), "addon");
    }

    #[test]
    fn slug_long_names() {
        let s = slugify("Black Autumn Playermodel Selector Extended Edition");
        assert_eq!(s, "black_autumn_playermodel_selecto");
        assert!(s.len() <= 32);
        let s = slugify("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbb");
        assert_eq!(s, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert!(!s.ends_with('_'));
    }

    #[test]
    fn existing_and_misnamed() {
        let root = crate::gma::tests::temp_test_dir("addons");
        for d in ["ws_123_old_name", "ws_1234_other", "my_pack_123", "ws_5_x", "plain"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        assert_eq!(find_existing(&root, 123), vec!["ws_123_old_name".to_string()]);
        assert_eq!(find_misnamed(&root, 123).as_deref(), Some("my_pack_123"));
        assert_eq!(find_misnamed(&root, 5), None);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn delete_only_inside_addons() {
        let root = crate::gma::tests::temp_test_dir("delete");
        let cfg = test_cfg(&root);
        fs::create_dir_all(cfg.addons_dir.join("ws_9_test").join("lua")).unwrap();
        fs::create_dir_all(root.join("outside")).unwrap();
        assert!(delete_addon_dir(&cfg, "..").is_err());
        assert!(delete_addon_dir(&cfg, "../outside").is_err());
        assert!(delete_addon_dir(&cfg, "").is_err());
        delete_addon_dir(&cfg, "ws_9_test").unwrap();
        assert!(!cfg.addons_dir.join("ws_9_test").exists());
        assert!(root.join("outside").exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn loader_created_once_and_never_overwritten() {
        let root = crate::gma::tests::temp_test_dir("loader");
        let cfg = test_cfg(&root);
        fs::create_dir_all(&cfg.addons_dir).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let r = Reporter::new(tx, Arc::new(|| {}));
        ensure_loader(&cfg, &r);
        let lua = cfg.addons_dir.join(LOADER_DIR).join("lua").join("autorun").join("server").join("workshop_drop_loader.lua");
        assert!(fs::read_to_string(&lua).unwrap().contains("resource.AddWorkshop(id)"));
        fs::write(&lua, "-- edited by user").unwrap();
        ensure_loader(&cfg, &r);
        assert_eq!(fs::read_to_string(&lua).unwrap(), "-- edited by user");

        let off = Config { create_loader: false, addons_dir: root.join("addons2"), ..cfg };
        fs::create_dir_all(&off.addons_dir).unwrap();
        ensure_loader(&off, &r);
        assert!(!off.addons_dir.join(LOADER_DIR).exists());
        fs::remove_dir_all(&root).ok();
    }

    /// End-to-end check with network and a real steamcmd (into a temp addons folder).
    /// `WD_E2E_STEAMCMD=<steamcmd folder> WD_E2E_IDS="104482086 ..." cargo test e2e -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn e2e_real_install() {
        let steamcmd = std::env::var("WD_E2E_STEAMCMD").expect("WD_E2E_STEAMCMD");
        let ids = std::env::var("WD_E2E_IDS").unwrap_or_else(|_| "104482086".into());
        let root = crate::gma::tests::temp_test_dir("e2e");
        let cfg = Config { steamcmd_dir: PathBuf::from(steamcmd), ..test_cfg(&root) };
        fs::create_dir_all(&cfg.addons_dir).unwrap();
        // WD_E2E_PRESEED=<folder name>: pretend an old version is already installed.
        if let Ok(old) = std::env::var("WD_E2E_PRESEED") {
            fs::create_dir_all(cfg.addons_dir.join(&old).join("lua")).unwrap();
            fs::write(cfg.addons_dir.join(&old).join("lua").join("old.lua"), b"-- old").unwrap();
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let r = Reporter::new(tx, Arc::new(|| {}));
        analyze(&cfg, &ids, false, &r);
        let mut plan = None;
        for ev in rx.try_iter() {
            match ev {
                Event::Log(s) => println!("LOG {s}"),
                Event::Progress { id, title, stage, note } => println!("PROGRESS {id} {title} {stage:?} {note}"),
                Event::Plan(p) => plan = Some(p),
                _ => {}
            }
        }
        let plan = plan.expect("plan");
        for it in &plan.items {
            println!("PLAN {} {} {:?} selected={} map={}", it.id, it.title, it.origin, it.selected, it.is_map);
        }
        if std::env::var("WD_E2E_ANALYZE_ONLY").is_ok() {
            return;
        }
        run_install(&cfg, plan.items, &r);
        for ev in rx.try_iter() {
            match ev {
                Event::Log(s) if !s.starts_with("[steamcmd]") => println!("LOG {s}"),
                Event::Progress { id, title, stage, note } => println!("PROGRESS {id} {title} {stage:?} {note}"),
                _ => {}
            }
        }
        for d in list_dirs(&cfg.addons_dir) {
            let (f, b) = dir_stats(&cfg.addons_dir.join(&d));
            println!("RESULT {d}: {f} files, {b} bytes");
        }
        println!("ROOT {}", root.display());
    }
}
