use std::{
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align, Button, CentralPanel, Color32, ColorImage, ComboBox, DragValue, Layout, Panel, RichText, Stroke,
    TextEdit, TextureHandle, TextureOptions, Ui, Vec2,
};
use itmo_labels_core::{
    fonts,
    render::{self, Orientation as RenderOrientation, Rendered, TextLabel},
    tspl::Tspl,
};

use crate::{
    config::{self, Lang, Orientation, Size, SizesFileWatch, State},
    i18n::{Tr, tr},
    printer::{Health, Snapshot, Worker},
};

const DPI: u32 = 203;
const SAVE_DELAY: Duration = Duration::from_secs(1);

/// Everything that changes the rendered label; a change triggers a re-render.
#[derive(PartialEq)]
struct RenderKey {
    size: Option<Size>,
    orientation: Orientation,
    bold: bool,
    size_pt: Option<f32>,
    text: String,
}

pub struct App {
    dir: PathBuf,
    state: State,
    saved: State,
    changed_at: Instant,
    sizes: Vec<Size>,
    sizes_watch: Option<SizesFileWatch>,
    sizes_error: Option<String>,
    worker: Worker,
    snapshot: Snapshot,
    rendered: Option<(RenderKey, Rendered, TextureHandle)>,
    regular: fonts::Font<'static>,
    bold: fonts::Font<'static>,
    #[cfg(debug_assertions)]
    screenshot: crate::debug::Screenshot,
}

impl App {
    pub fn new(ctx: &egui::Context) -> Self {
        let dir = config::dir();
        let state = State::load(&dir);
        let (sizes_watch, sizes_error) = match SizesFileWatch::open(&dir) {
            Ok(w) => (Some(w), None),
            Err(e) => (None, Some(format!("{}: {e}", dir.display()))),
        };
        let wake = ctx.clone();
        let worker = Worker::spawn(state.printer.clone(), move || wake.request_repaint());
        Self {
            dir,
            saved: state.clone(),
            state,
            changed_at: Instant::now(),
            sizes: config::builtin_sizes(),
            sizes_watch,
            sizes_error,
            worker,
            snapshot: Snapshot::default(),
            rendered: None,
            regular: fonts::regular(),
            bold: fonts::bold(),
            #[cfg(debug_assertions)]
            screenshot: crate::debug::Screenshot::from_env(),
        }
    }

    fn lang(&self) -> Lang {
        self.state.lang.unwrap_or_else(Lang::from_system)
    }

    fn selected_size(&self) -> Option<&Size> {
        let wanted = self.state.size.as_deref();
        self.sizes.iter().find(|s| Some(s.title().as_str()) == wanted).or(self.sizes.first())
    }

    fn render_key(&self) -> RenderKey {
        RenderKey {
            size: self.selected_size().cloned(),
            orientation: self.state.orientation,
            bold: self.state.bold,
            size_pt: (!self.state.auto_size).then_some(self.state.size_pt),
            text: self.state.text.clone(),
        }
    }

    fn rerender_if_needed(&mut self, ctx: &egui::Context) {
        let key = self.render_key();
        if self.rendered.as_ref().is_some_and(|(k, ..)| *k == key) {
            return;
        }
        let Some(size) = &key.size else {
            self.rendered = None;
            return;
        };
        let font = if key.bold { &self.bold } else { &self.regular };
        let r = render::render(&TextLabel {
            width_mm: size.width_mm,
            height_mm: size.height_mm,
            gap_mm: size.gap_mm,
            dpi: DPI,
            orientation: match key.orientation {
                Orientation::Portrait => RenderOrientation::Portrait,
                Orientation::Landscape => RenderOrientation::Landscape,
            },
            font,
            text: &key.text,
            size_pt: key.size_pt,
        });
        let b = &r.preview;
        let pixels = (0..b.height_px).flat_map(|y| (0..b.width_px).map(move |x| if b.get(x, y) { 0 } else { 255 }));
        let image = ColorImage::from_gray_iter([b.width_px as usize, b.height_px as usize], pixels);
        let texture = ctx.load_texture("label-preview", image, TextureOptions::LINEAR);
        if self.state.auto_size {
            // Seed the manual size with the fitted one, so unticking "Auto" starts from what is on screen.
            self.state.size_pt = (r.size_pt * 2.0).round() / 2.0;
        }
        self.rendered = Some((key, r, texture));
    }

    fn poll_sizes(&mut self) {
        let Some(watch) = &mut self.sizes_watch else { return };
        match watch.poll() {
            Some(Ok(sizes)) => {
                self.sizes = sizes;
                self.sizes_error = None;
            }
            Some(Err(e)) => self.sizes_error = Some(e),
            None => {}
        }
    }

    fn save_state_if_settled(&mut self) {
        if self.state != self.saved {
            if self.changed_at.elapsed() >= SAVE_DELAY {
                let _ = self.state.save(&self.dir);
                self.saved = self.state.clone();
            }
        } else {
            self.changed_at = Instant::now();
        }
    }

    fn open_sizes_file(&self) {
        let Some(watch) = &self.sizes_watch else { return };
        let path = watch.path.as_os_str();
        let _ = if cfg!(target_os = "macos") {
            Command::new("open").arg("-t").arg(path).spawn()
        } else if cfg!(windows) {
            Command::new("notepad").arg(path).spawn()
        } else {
            Command::new("xdg-open").arg(path).spawn()
        };
    }

    fn controls(&mut self, ui: &mut Ui, t: &Tr) {
        ui.spacing_mut().item_spacing.y = 6.0;

        ui.horizontal(|ui| {
            ui.heading("ITMO Labels");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut lang = self.lang();
                ui.selectable_value(&mut lang, Lang::En, "EN");
                ui.selectable_value(&mut lang, Lang::Ru, "RU");
                if lang != self.lang() {
                    self.state.lang = Some(lang);
                }
            });
        });
        ui.add_space(8.0);

        ui.label(t.printer);
        let printers = self.snapshot.printers.clone();
        let current = self.state.printer.clone();
        let shown = match (&current, printers.is_empty()) {
            (Some(name), _) => name.as_str(),
            (None, true) => t.no_printers,
            (None, false) => t.choose_printer,
        };
        ComboBox::from_id_salt("printer").width(ui.available_width()).selected_text(shown).show_ui(ui, |ui| {
            for name in &printers {
                if ui.selectable_label(current.as_deref() == Some(name), name).clicked() {
                    self.state.printer = Some(name.clone());
                    self.worker.select(Some(name.clone()));
                }
            }
        });
        if current.is_some() {
            status_line(ui, t, &self.snapshot);
        }
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.label(t.size);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button(t.edit_sizes).clicked() {
                    self.open_sizes_file();
                }
            });
        });
        let selected = self.selected_size().map(Size::title).unwrap_or_default();
        ComboBox::from_id_salt("size").width(ui.available_width()).selected_text(&selected).show_ui(ui, |ui| {
            for s in &self.sizes {
                let title = s.title();
                if ui.selectable_label(title == selected, &title).clicked() {
                    self.state.size = Some(title);
                }
            }
        });
        if let Some(e) = &self.sizes_error {
            ui.colored_label(ui.visuals().warn_fg_color, format!("{} {e}", t.sizes_error));
        }
        ui.add_space(8.0);

        ui.label(t.orientation);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.state.orientation, Orientation::Portrait, t.portrait);
            ui.selectable_value(&mut self.state.orientation, Orientation::Landscape, t.landscape);
        });
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.label(t.font_size);
            ui.checkbox(&mut self.state.auto_size, t.auto);
            ui.add_enabled(
                !self.state.auto_size,
                DragValue::new(&mut self.state.size_pt).range(4.0..=400.0).speed(0.5).suffix(" pt"),
            );
            ui.checkbox(&mut self.state.bold, t.bold);
        });
        ui.add_space(8.0);

        ui.label(t.text);
        ui.add(TextEdit::singleline(&mut self.state.text).hint_text(t.text_hint).desired_width(f32::INFINITY));
        if self.rendered.as_ref().is_some_and(|(_, r, _)| r.overflows) {
            ui.colored_label(ui.visuals().warn_fg_color, t.overflow);
        }
        ui.add_space(12.0);

        let ready = self.snapshot.health == Some(Health::Ready) && !self.snapshot.busy_printing;
        let can_print = ready && !self.state.text.trim().is_empty() && self.rendered.is_some();
        let label = if self.snapshot.busy_printing { t.printing } else { t.print };
        let button = Button::new(RichText::new(label).size(18.0)).min_size(Vec2::new(ui.available_width(), 40.0));
        if ui.add_enabled(can_print, button).clicked()
            && let Some((_, r, _)) = &self.rendered
            && let Ok(job) = (Tspl { dpi: DPI }).encode(&r.label, 1)
        {
            self.worker.print(job);
        }
        match &self.snapshot.last_job {
            Some(Ok(())) => {
                ui.label(t.printed);
            }
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, format!("{} {e}", t.print_failed));
            }
            None => {}
        }
    }

    fn preview(&self, ui: &mut Ui) {
        let Some((_, r, texture)) = &self.rendered else { return };
        let (w, h) = (r.preview.width_px as f32, r.preview.height_px as f32);
        let area = ui.available_rect_before_wrap();
        ui.painter().rect_filled(area, 0.0, Color32::from_gray(200));
        let avail = area.size() - Vec2::splat(48.0);
        let scale = (avail.x / w).min(avail.y / h).max(0.05);
        let rect = egui::Rect::from_center_size(area.center(), Vec2::new(w, h) * scale);
        ui.painter().rect_filled(rect.translate(Vec2::new(2.0, 3.0)), 3.0, Color32::from_black_alpha(50));
        egui::Image::new(texture).corner_radius(3.0).paint_at(ui, rect);
        ui.painter().rect_stroke(rect, 3.0, Stroke::new(1.0, Color32::from_gray(150)), egui::StrokeKind::Outside);
    }
}

fn status_line(ui: &mut Ui, t: &Tr, s: &Snapshot) {
    let (text, color) = match &s.health {
        None => (t.checking.to_owned(), Color32::GRAY),
        Some(Health::Ready) => (t.ready.to_owned(), Color32::from_rgb(0x2e, 0xa0, 0x43)),
        Some(Health::Printing) => (t.busy_printing.to_owned(), Color32::from_rgb(0x2f, 0x81, 0xf7)),
        Some(Health::NoPaper) => (t.no_paper.to_owned(), Color32::from_rgb(0xd2, 0x99, 0x22)),
        Some(Health::HeadOpen) => (t.head_open.to_owned(), Color32::from_rgb(0xd2, 0x99, 0x22)),
        Some(Health::PaperJam) => (t.paper_jam.to_owned(), Color32::from_rgb(0xd2, 0x99, 0x22)),
        Some(Health::Paused) => (t.paused.to_owned(), Color32::from_rgb(0xd2, 0x99, 0x22)),
        Some(Health::Error(b)) => (format!("{} ({b:#04x})", t.error), Color32::from_rgb(0xda, 0x36, 0x33)),
        Some(Health::NoReply) => (t.no_reply.to_owned(), Color32::from_rgb(0xda, 0x36, 0x33)),
        Some(Health::Busy) => (t.busy.to_owned(), Color32::from_rgb(0xd2, 0x99, 0x22)),
        Some(Health::Unavailable(e)) => (format!("{}: {e}", t.unavailable), Color32::from_rgb(0xda, 0x36, 0x33)),
        Some(Health::Disconnected) => (t.disconnected.to_owned(), Color32::GRAY),
    };
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 5.0, color);
        ui.label(text);
    });
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.snapshot = self.worker.snapshot();
        if self.state.printer.is_none()
            && let Some(first) = self.snapshot.printers.first()
        {
            self.state.printer = Some(first.clone());
            self.worker.select(Some(first.clone()));
        }
        self.poll_sizes();
        self.rerender_if_needed(ctx);
        self.save_state_if_settled();
        // Keep polling sizes.toml and flushing state even when nothing else repaints.
        ctx.request_repaint_after(Duration::from_secs(1));
        #[cfg(debug_assertions)]
        self.screenshot.tick(ctx);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let t = tr(self.lang());
        Panel::left("controls").exact_size(320.0).show(ui, |ui| {
            ui.add_space(8.0);
            self.controls(ui, t);
        });
        CentralPanel::default().show(ui, |ui| self.preview(ui));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.state.save(&self.dir);
    }
}
