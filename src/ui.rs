//! UI (egui): tabs "Install", "Installed", "Settings", log at the bottom.

use crate::config::{self, Config};
use crate::install::{self, fmt_size, Event, InstalledAddon, Origin, Plan, PlanItem, Reporter, Stage};
use eframe::egui::{self, Color32, RichText};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

const GREEN: Color32 = Color32::from_rgb(90, 200, 110);
const YELLOW: Color32 = Color32::from_rgb(235, 190, 70);
const RED: Color32 = Color32::from_rgb(235, 90, 90);
const GRAY: Color32 = Color32::from_rgb(120, 120, 120);

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Install,
    Installed,
    Settings,
}

struct ProgressRow {
    id: u64,
    title: String,
    stage: Stage,
    error: Option<String>,
    note: String,
}

enum InstalledAction {
    OpenWorkshop(u64),
    OpenFolder(String),
    Update(u64),
    Remove(InstalledAddon),
}

pub struct App {
    cfg: Option<Config>,
    /// Settings fields: server, addons, steamcmd.
    draft: [String; 3],
    draft_loader: bool,
    settings_msg: Option<(bool, String)>,
    tab: Tab,
    input: String,
    busy: bool,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    log: Vec<String>,
    rows: Vec<ProgressRow>,
    plan: Option<Plan>,
    installed: Vec<InstalledAddon>,
    installed_loading: bool,
    remove_confirm: Option<InstalledAddon>,
    ctx: egui::Context,
}

fn draft_from(cfg: &Config) -> [String; 3] {
    let s = |p: &PathBuf| if p.as_os_str().is_empty() { String::new() } else { p.display().to_string() };
    [s(&cfg.server_dir), s(&cfg.addons_dir), s(&cfg.steamcmd_dir)]
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = channel();
        let mut app = App {
            cfg: None,
            draft: Default::default(),
            draft_loader: true,
            settings_msg: None,
            tab: Tab::Install,
            input: String::new(),
            busy: false,
            tx,
            rx,
            log: Vec::new(),
            rows: Vec::new(),
            plan: None,
            installed: Vec::new(),
            installed_loading: false,
            remove_confirm: None,
            ctx: cc.egui_ctx.clone(),
        };
        app.push_log(format!("{} {}", crate::APP_NAME, crate::APP_VERSION));
        match config::load() {
            Ok(Some(cfg)) => {
                app.draft = draft_from(&cfg);
                app.draft_loader = cfg.create_loader;
                if let Err(e) = cfg.validate() {
                    app.push_log(format!("Warning: {e:#}. Check the Settings tab."));
                }
                app.cfg = Some(cfg);
                app.refresh_installed();
            }
            Ok(None) => {
                app.draft = draft_from(&Config::suggested());
                app.tab = Tab::Settings;
                app.settings_msg = Some((true, "First run: pick your folders with \"Browse...\" and press \"Save\".".into()));
            }
            Err(e) => {
                app.draft = draft_from(&Config::suggested());
                app.tab = Tab::Settings;
                app.settings_msg = Some((false, format!("Config error: {e:#}")));
            }
        }
        app
    }

    fn reporter(&self) -> Reporter {
        let ctx = self.ctx.clone();
        Reporter::new(self.tx.clone(), Arc::new(move || ctx.request_repaint()))
    }

    fn push_log(&mut self, s: String) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        for line in s.lines() {
            self.log.push(format!("[{ts}] {line}"));
        }
        if self.log.len() > 20_000 {
            self.log.drain(..5_000);
        }
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Event::Log(s) => self.push_log(s),
                Event::Progress { id, title, stage, note } => {
                    let idx = match self.rows.iter().position(|r| r.id == id) {
                        Some(i) => i,
                        None => {
                            self.rows.push(ProgressRow {
                                id,
                                title: title.clone(),
                                stage: Stage::Info,
                                error: None,
                                note: String::new(),
                            });
                            self.rows.len() - 1
                        }
                    };
                    let row = &mut self.rows[idx];
                    if !title.is_empty() {
                        row.title = title;
                    }
                    row.note = note.clone();
                    if stage == Stage::Error {
                        row.error = Some(note);
                        row.note.clear();
                    } else {
                        row.stage = stage;
                        row.error = None;
                    }
                }
                Event::Plan(plan) => {
                    self.busy = false;
                    if plan.needs_confirmation {
                        self.push_log("Review the list and press \"Install selected\".".into());
                        self.plan = Some(plan);
                    } else {
                        self.start_install(plan.items);
                    }
                }
                Event::Installed(list) => {
                    self.installed = list;
                    self.installed_loading = false;
                }
                Event::Finished => {
                    self.busy = false;
                    self.refresh_installed();
                }
            }
        }
    }

    // ------------------------------------------------------------ background jobs

    fn start_analyze(&mut self, input: String, force_update: bool) {
        let Some(cfg) = self.cfg.clone() else { return };
        self.busy = true;
        self.plan = None;
        self.rows.clear();
        let r = self.reporter();
        std::thread::spawn(move || install::analyze(&cfg, &input, force_update, &r));
    }

    fn start_install(&mut self, items: Vec<PlanItem>) {
        let Some(cfg) = self.cfg.clone() else { return };
        if !items.iter().any(|i| i.selected) {
            self.push_log("Nothing selected.".into());
            return;
        }
        self.busy = true;
        let r = self.reporter();
        std::thread::spawn(move || install::run_install(&cfg, items, &r));
    }

    fn refresh_installed(&mut self) {
        let Some(cfg) = self.cfg.clone() else { return };
        if self.installed_loading {
            return;
        }
        self.installed_loading = true;
        let r = self.reporter();
        std::thread::spawn(move || install::scan_installed(&cfg, &r));
    }

    fn start_remove(&mut self, a: InstalledAddon) {
        let Some(cfg) = self.cfg.clone() else { return };
        self.busy = true;
        let r = self.reporter();
        std::thread::spawn(move || install::remove_addon(&cfg, &a.dir_name, &r));
    }

    // ------------------------------------------------------------ tabs

    fn ui_install(&mut self, ui: &mut egui::Ui) {
        ui.heading("Steam Workshop links");
        ui.label("One per line. Full links, plain ids and collections all work.");
        ui.add(
            egui::TextEdit::multiline(&mut self.input)
                .desired_rows(4)
                .desired_width(f32::INFINITY)
                .hint_text("https://steamcommunity.com/sharedfiles/filedetails/?id=104482086"),
        );
        ui.horizontal(|ui| {
            let can = !self.busy && self.plan.is_none() && self.cfg.is_some() && !self.input.trim().is_empty();
            let btn = egui::Button::new(RichText::new("Install to server").strong().size(16.0));
            if ui.add_enabled(can, btn).clicked() {
                let input = self.input.clone();
                self.start_analyze(input, false);
            }
            if ui.add_enabled(!self.busy, egui::Button::new("Clear")).clicked() {
                self.input.clear();
                self.rows.clear();
                self.plan = None;
            }
            if self.cfg.is_none() {
                ui.colored_label(RED, "Set up your folders in the Settings tab first.");
            }
        });
        ui.separator();

        if self.plan.is_some() {
            self.ui_plan(ui);
            ui.separator();
        }
        self.ui_progress(ui);
    }

    fn ui_plan(&mut self, ui: &mut egui::Ui) {
        let mut action: Option<bool> = None;
        let Some(plan) = self.plan.as_mut() else { return };
        ui.label(RichText::new("Check what to install:").strong().size(15.0));
        for w in &plan.warnings {
            ui.colored_label(YELLOW, w);
        }
        if plan.items.len() > 1 {
            ui.horizontal(|ui| {
                if ui.small_button("Select all").clicked() {
                    plan.items.iter_mut().for_each(|i| i.selected = true);
                }
                if ui.small_button("Select none").clicked() {
                    plan.items.iter_mut().for_each(|i| i.selected = false);
                }
            });
        }
        egui::ScrollArea::vertical()
            .id_salt("plan_scroll")
            .max_height(300.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for it in plan.items.iter_mut() {
                    ui.horizontal(|ui| {
                        let label = format!("{} ({})", it.title, it.id);
                        ui.checkbox(&mut it.selected, RichText::new(label).strong());
                        if it.size > 0 {
                            ui.weak(fmt_size(it.size));
                        }
                        match &it.origin {
                            Origin::Direct => {}
                            Origin::Collection(c) => {
                                ui.weak(format!("from collection \"{c}\""));
                            }
                            Origin::Dependency(p) => {
                                ui.colored_label(YELLOW, format!("required by \"{p}\""));
                            }
                        }
                    });
                    ui.indent(it.id, |ui| {
                        if !it.existing.is_empty() {
                            ui.colored_label(
                                YELLOW,
                                format!("Already installed: {}. Update? (the old folder will be replaced)", it.existing.join(", ")),
                            );
                        }
                        if it.is_map {
                            ui.colored_label(
                                RED,
                                "This is a map. Maps are better added to your Steam Workshop collection, not to server \
                                 content. Tick the box if you still want it.",
                            );
                        }
                        if let (Some(m), true) = (&it.misnamed, it.existing.is_empty()) {
                            let label = format!(
                                "Rename the existing folder \"{m}\" to ws_{}_{} (no download)",
                                it.id,
                                install::slugify(&it.title)
                            );
                            ui.checkbox(&mut it.rename_instead, label);
                        }
                    });
                }
            });
        let n = plan.items.iter().filter(|i| i.selected).count();
        ui.horizontal(|ui| {
            let btn = egui::Button::new(RichText::new(format!("Install selected ({n})")).strong());
            if ui.add_enabled(n > 0 && !self.busy, btn).clicked() {
                action = Some(true);
            }
            if ui.button("Cancel").clicked() {
                action = Some(false);
            }
        });
        match action {
            Some(true) => {
                if let Some(plan) = self.plan.take() {
                    self.start_install(plan.items);
                }
            }
            Some(false) => {
                self.plan = None;
                self.push_log("Cancelled.".into());
            }
            None => {}
        }
    }

    fn ui_progress(&mut self, ui: &mut egui::Ui) {
        if self.rows.is_empty() {
            return;
        }
        ui.label(RichText::new("Progress").strong());
        egui::ScrollArea::vertical()
            .id_salt("progress_scroll")
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Grid::new("progress_grid").striped(true).num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                    for row in &self.rows {
                        ui.label(format!("{} ({})", row.title, row.id));
                        ui.horizontal(|ui| {
                            let steps = [(Stage::Info, "Info"), (Stage::Download, "Download"), (Stage::Extract, "Extract")];
                            let cur = stage_index(row.stage);
                            for (st, name) in steps {
                                let idx = stage_index(st);
                                let color = if row.error.is_some() && row.stage == st {
                                    RED
                                } else if cur > idx {
                                    GREEN
                                } else if cur == idx && row.error.is_none() {
                                    YELLOW
                                } else {
                                    GRAY
                                };
                                ui.label(RichText::new(name).color(color));
                                ui.label(RichText::new("→").color(GRAY));
                            }
                            if let Some(e) = &row.error {
                                ui.label(RichText::new("Error").color(RED).strong());
                                ui.label(RichText::new(e).color(RED));
                            } else if row.stage == Stage::Done {
                                ui.label(RichText::new("Done").color(GREEN).strong());
                                if !row.note.is_empty() {
                                    ui.weak(&row.note);
                                }
                            } else {
                                ui.label(RichText::new("Done").color(GRAY));
                                if self.busy {
                                    ui.spinner();
                                }
                            }
                        });
                        ui.end_row();
                    }
                });
            });
    }

    fn ui_installed(&mut self, ui: &mut egui::Ui) {
        let mut action: Option<InstalledAction> = None;
        ui.horizontal(|ui| {
            ui.heading(format!("Installed Workshop addons ({})", self.installed.len()));
            if ui
                .add_enabled(!self.installed_loading && self.cfg.is_some(), egui::Button::new("Refresh"))
                .clicked()
            {
                self.refresh_installed();
            }
            if self.installed_loading {
                ui.spinner();
            }
        });
        let total: u64 = self.installed.iter().map(|a| a.size).sum();
        ui.weak(format!("Total: {}", fmt_size(total)));
        ui.separator();
        egui::ScrollArea::both().id_salt("installed_scroll").auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("installed_grid").striped(true).num_columns(5).spacing([14.0, 6.0]).show(ui, |ui| {
                ui.strong("ID");
                ui.strong("Title / folder");
                ui.strong("Size");
                ui.strong("Date");
                ui.strong("");
                ui.end_row();
                for a in &self.installed {
                    ui.monospace(a.id.to_string());
                    ui.vertical(|ui| {
                        ui.label(&a.title);
                        ui.weak(RichText::new(&a.dir_name).small());
                    });
                    ui.label(format!("{} · {} files", fmt_size(a.size), a.files));
                    ui.label(&a.modified);
                    ui.horizontal(|ui| {
                        if ui.button("Open in Workshop").clicked() {
                            action = Some(InstalledAction::OpenWorkshop(a.id));
                        }
                        if ui.button("Folder").clicked() {
                            action = Some(InstalledAction::OpenFolder(a.dir_name.clone()));
                        }
                        if ui.add_enabled(!self.busy, egui::Button::new("Update")).clicked() {
                            action = Some(InstalledAction::Update(a.id));
                        }
                        if ui.add_enabled(!self.busy, egui::Button::new("Remove")).clicked() {
                            action = Some(InstalledAction::Remove(a.clone()));
                        }
                    });
                    ui.end_row();
                }
            });
        });

        match action {
            Some(InstalledAction::OpenWorkshop(id)) => {
                open_external(&format!("https://steamcommunity.com/sharedfiles/filedetails/?id={id}"));
            }
            Some(InstalledAction::OpenFolder(dir)) => {
                if let Some(cfg) = &self.cfg {
                    open_external(&cfg.addons_dir.join(dir).display().to_string());
                }
            }
            Some(InstalledAction::Update(id)) => {
                self.input = id.to_string();
                self.tab = Tab::Install;
                self.start_analyze(id.to_string(), true);
            }
            Some(InstalledAction::Remove(a)) => self.remove_confirm = Some(a),
            None => {}
        }
    }

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("Settings");
        if let Some((ok, msg)) = &self.settings_msg {
            ui.colored_label(if *ok { GREEN } else { RED }, msg);
        }
        ui.add_space(6.0);
        let labels = [
            "Server folder (has srcds_win64.exe)",
            "Server addons folder",
            "steamcmd folder (has steamcmd.exe)",
        ];
        let mut picked: Option<(usize, PathBuf)> = None;
        egui::Grid::new("settings_grid").num_columns(3).spacing([10.0, 8.0]).show(ui, |ui| {
            for (i, label) in labels.iter().enumerate() {
                ui.label(*label);
                ui.add_sized([520.0, 22.0], egui::TextEdit::singleline(&mut self.draft[i]));
                if ui.button("Browse...").clicked() {
                    let mut dlg = rfd::FileDialog::new().set_title(*label);
                    let cur = PathBuf::from(&self.draft[i]);
                    if cur.is_dir() {
                        dlg = dlg.set_directory(&cur);
                    }
                    if let Some(p) = dlg.pick_folder() {
                        picked = Some((i, p));
                    }
                }
                ui.end_row();
            }
        });
        if let Some((i, p)) = picked {
            self.draft[i] = p.display().to_string();
            // Picked the server folder -> fill addons automatically.
            if i == 0 {
                let addons = p.join("garrysmod").join("addons");
                if addons.is_dir() {
                    self.draft[1] = addons.display().to_string();
                }
            }
        }
        ui.add_space(6.0);
        ui.checkbox(
            &mut self.draft_loader,
            "Create a loader addon so players download installed addons on join (resource.AddWorkshop)",
        );
        ui.weak(format!(
            "It is a tiny addons\\{} folder. Turn this off if your server already does resource.AddWorkshop itself.",
            install::LOADER_DIR
        ));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button(RichText::new("Save").strong()).clicked() {
                self.save_settings();
            }
            if let Some(cfg) = &self.cfg {
                if ui.button("Open addons folder").clicked() {
                    open_external(&cfg.addons_dir.display().to_string());
                }
            }
        });
        ui.add_space(8.0);
        ui.weak(format!("Config file: {}", config::config_path().display()));
        ui.weak(format!("Temp folder: {}", Config::temp_dir().display()));
        ui.weak(format!("{} {}", crate::APP_NAME, crate::APP_VERSION));
    }

    fn save_settings(&mut self) {
        let cfg = Config {
            server_dir: PathBuf::from(self.draft[0].trim()),
            addons_dir: PathBuf::from(self.draft[1].trim()),
            steamcmd_dir: PathBuf::from(self.draft[2].trim()),
            create_loader: self.draft_loader,
        };
        match cfg.validate() {
            Err(e) => self.settings_msg = Some((false, format!("Not saved: {e:#}"))),
            Ok(warnings) => match config::save(&cfg) {
                Ok(()) => {
                    let mut msg = "Saved.".to_string();
                    for w in warnings {
                        msg.push_str(&format!(" Warning: {w}"));
                    }
                    self.settings_msg = Some((true, msg));
                    self.push_log(format!("Settings saved to {}", config::config_path().display()));
                    self.cfg = Some(cfg);
                    self.installed_loading = false;
                    self.refresh_installed();
                }
                Err(e) => self.settings_msg = Some((false, format!("{e:#}"))),
            },
        }
    }

    fn ui_log(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Log");
            if ui.button("Copy log").clicked() {
                ui.ctx().copy_text(self.log.join("\r\n"));
            }
            if ui.button("Clear").clicked() {
                self.log.clear();
            }
        });
        let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
        egui::ScrollArea::both()
            .id_salt("log_scroll")
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show_rows(ui, row_h, self.log.len(), |ui, range| {
                for line in &self.log[range] {
                    let color = if line.contains("Error") || line.contains("ERROR") {
                        RED
                    } else if line.contains("Warning") {
                        YELLOW
                    } else if line.contains("] Done") || line.contains("] Installed") {
                        GREEN
                    } else if line.contains("[steamcmd]") {
                        GRAY
                    } else {
                        ui.visuals().text_color()
                    };
                    ui.add(
                        egui::Label::new(RichText::new(line).monospace().color(color))
                            .wrap_mode(egui::TextWrapMode::Extend),
                    );
                }
            });
    }
}

fn stage_index(s: Stage) -> u8 {
    match s {
        Stage::Info => 0,
        Stage::Download => 1,
        Stage::Extract => 2,
        Stage::Done => 3,
        Stage::Error => 0,
    }
}

/// Opens a URL or folder with the system handler.
fn open_external(target: &str) {
    let _ = std::process::Command::new("explorer").arg(target).spawn();
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Install, RichText::new("Install").size(15.0));
                ui.selectable_value(&mut self.tab, Tab::Installed, RichText::new("Installed").size(15.0));
                ui.selectable_value(&mut self.tab, Tab::Settings, RichText::new("Settings").size(15.0));
                if self.busy {
                    ui.separator();
                    ui.spinner();
                    ui.label("Working...");
                }
            });
            ui.add_space(2.0);
        });

        egui::TopBottomPanel::bottom("log_panel")
            .resizable(true)
            .default_height(230.0)
            .min_height(90.0)
            .show(ctx, |ui| self.ui_log(ui));

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Install => self.ui_install(ui),
            Tab::Installed => self.ui_installed(ui),
            Tab::Settings => self.ui_settings(ui),
        });

        if let Some(a) = self.remove_confirm.clone() {
            let mut go = false;
            let mut close = false;
            egui::Window::new("Remove addon?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!("\"{}\"\n{}", a.title, a.dir_name));
                    ui.colored_label(RED, "The folder will be deleted permanently.");
                    ui.horizontal(|ui| {
                        if ui.button("Delete").clicked() {
                            go = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            if go {
                self.remove_confirm = None;
                self.start_remove(a);
            } else if close {
                self.remove_confirm = None;
            }
        }
    }
}
