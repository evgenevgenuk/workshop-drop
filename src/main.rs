// No console window (tests keep the normal subsystem so output is visible).
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod config;
mod gma;
mod install;
mod steam_api;
mod steamcmd;
mod ui;

pub const APP_NAME: &str = "WorkshopDrop";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> eframe::Result<()> {
    let title = format!("{APP_NAME} {APP_VERSION}");
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title(&title)
            .with_inner_size([1000.0, 780.0])
            .with_min_inner_size([720.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(&title, options, Box::new(|cc| Ok(Box::new(ui::App::new(cc)))))
}
