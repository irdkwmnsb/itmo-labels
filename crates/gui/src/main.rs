//! ITMO Labels: one line of text onto a USB label printer, no drivers needed.

// No console window on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
#[cfg(debug_assertions)]
mod debug;
mod editor;
mod i18n;
mod printer;

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};
use itmo_labels_core::fonts;

/// egui is built without its default fonts; the embedded Helvetica draws the UI too.
fn install_fonts(ctx: &egui::Context) {
    let mut defs = FontDefinitions::empty();
    defs.font_data.insert("helvetica".into(), FontData::from_static(fonts::HELVETICA_REGULAR).into());
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        defs.families.insert(family, vec!["helvetica".into()]);
    }
    ctx.set_fonts(defs);
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ITMO Labels")
            .with_inner_size([860.0, 560.0])
            .with_min_inner_size([640.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "itmo-labels",
        options,
        Box::new(|cc| {
            install_fonts(&cc.egui_ctx);
            Ok(Box::new(app::App::new(&cc.egui_ctx)))
        }),
    )
}
