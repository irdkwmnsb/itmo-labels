//! ITMO Labels: one line of text onto a USB label printer, no drivers needed.
//! The same app also builds for the web, where it is served by another copy in server mode.

// No console window on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
#[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
mod debug;
mod editor;
mod i18n;
mod printer;
mod query;
#[cfg(not(target_arch = "wasm32"))]
mod server;
#[cfg(not(target_arch = "wasm32"))]
mod serving;
#[cfg(target_arch = "wasm32")]
mod web;

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

#[cfg(not(target_arch = "wasm32"))]
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

/// Started by `index.html`; draws into its `<canvas id="app">`.
#[cfg(target_arch = "wasm32")]
fn main() {
    use wasm_bindgen::JsCast;

    wasm_bindgen_futures::spawn_local(async {
        let document = web_sys::window().and_then(|w| w.document()).expect("runs in a page");
        let canvas = document
            .get_element_by_id("app")
            .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
            .expect("index.html has a canvas#app");
        let started = eframe::WebRunner::new()
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(|cc| {
                    install_fonts(&cc.egui_ctx);
                    Ok(Box::new(app::App::new(&cc.egui_ctx)))
                }),
            )
            .await;
        if let Some(loading) = document.get_element_by_id("loading") {
            match started {
                Ok(()) => loading.remove(),
                Err(e) => loading.set_text_content(Some(&format!("Failed to start: {e:?}"))),
            }
        }
    });
}
