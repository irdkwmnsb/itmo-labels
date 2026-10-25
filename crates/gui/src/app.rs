use std::{
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align, Button, CentralPanel, Color32, ColorImage, ComboBox, Layout, Panel, RichText, Stroke, TextEdit,
    TextureHandle, TextureOptions, Ui, Vec2,
};
use itmo_labels_core::{
    fonts,
    render::{self, LabelSpec, Orientation as RenderOrientation, Rendered, TextField, Zone},
    tspl::Tspl,
};

use crate::{
    config::{self, Lang, Orientation, Size, State, Template, WatchedFile},
    i18n::{Tr, tr},
    printer::{Health, Snapshot, Worker},
};

const DPI: u32 = 203;
const SAVE_DELAY: Duration = Duration::from_secs(1);

/// Everything that changes the rendered label; a change triggers a re-render.
#[derive(PartialEq)]
struct RenderKey {
    size: Size,
    template: Template,
    orientation: Orientation,
    texts: Vec<String>,
}

/// A user-editable list that keeps its last valid contents while the file is broken.
struct Watched<T> {
    items: Vec<T>,
    file: Option<WatchedFile<Vec<T>>>,
    error: Option<String>,
}

impl<T> Watched<T> {
    fn new(builtin: Vec<T>, file: std::io::Result<WatchedFile<Vec<T>>>) -> Self {
        match file {
            Ok(file) => Self { items: builtin, file: Some(file), error: None },
            Err(e) => Self { items: builtin, file: None, error: Some(e.to_string()) },
        }
    }

    fn poll(&mut self) {
        match self.file.as_mut().and_then(WatchedFile::poll) {
            Some(Ok(items)) => {
                self.items = items;
                self.error = None;
            }
            Some(Err(e)) => self.error = Some(e),
            None => {}
        }
    }

    fn path(&self) -> Option<&Path> {
        self.file.as_ref().map(|f| f.path.as_path())
    }
}

pub struct App {
    dir: PathBuf,
    state: State,
    saved: State,
    changed_at: Instant,
    sizes: Watched<Size>,
    templates: Watched<Template>,
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
        let wake = ctx.clone();
        let worker = Worker::spawn(state.printer.clone(), move || wake.request_repaint());
        Self {
            sizes: Watched::new(config::builtin_sizes(), config::sizes_file(&dir)),
            templates: Watched::new(config::builtin_templates(), config::templates_file(&dir)),
            dir,
            saved: state.clone(),
            state,
            changed_at: Instant::now(),
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
        self.sizes.items.iter().find(|s| Some(s.title().as_str()) == wanted).or(self.sizes.items.first())
    }

    fn selected_template(&self) -> Option<&Template> {
        let wanted = self.state.template.as_deref();
        self.templates.items.iter().find(|t| Some(t.name.as_str()) == wanted).or(self.templates.items.first())
    }

    /// Last choice for this template, else the template's own default.
    fn orientation(&self, template: &Template) -> Orientation {
        self.state
            .templates
            .get(&template.name)
            .and_then(|s| s.orientation)
            .or(template.orientation)
            .unwrap_or_default()
    }

    fn texts(&self, template: &Template) -> Vec<String> {
        let saved = self.state.templates.get(&template.name);
        template
            .fields
            .iter()
            .map(|f| saved.and_then(|s| s.fields.get(&f.name)).cloned().unwrap_or_default())
            .collect()
    }

    fn render_key(&self) -> Option<RenderKey> {
        let template = self.selected_template()?.clone();
        Some(RenderKey {
            size: self.selected_size()?.clone(),
            orientation: self.orientation(&template),
            texts: self.texts(&template),
            template,
        })
    }

    fn rerender_if_needed(&mut self, ctx: &egui::Context) {
        let Some(key) = self.render_key() else {
            self.rendered = None;
            return;
        };
        if self.rendered.as_ref().is_some_and(|(k, ..)| *k == key) {
            return;
        }
        let fields: Vec<TextField> = key
            .template
            .fields
            .iter()
            .zip(&key.texts)
            .map(|(f, text)| TextField {
                text,
                font: if f.bold { &self.bold } else { &self.regular },
                zone: Zone { x: f.x, y: f.y, w: f.w, h: f.h },
                align: f.align.into(),
                fit: f.fit.into(),
                max_pt: f.max_pt,
            })
            .collect();
        let r = render::render(&LabelSpec {
            width_mm: key.size.width_mm,
            height_mm: key.size.height_mm,
            gap_mm: key.size.gap_mm,
            dpi: DPI,
            orientation: match key.orientation {
                Orientation::Portrait => RenderOrientation::Portrait,
                Orientation::Landscape => RenderOrientation::Landscape,
            },
            fields: &fields,
        });
        let b = &r.preview;
        let pixels = (0..b.height_px).flat_map(|y| (0..b.width_px).map(move |x| if b.get(x, y) { 0 } else { 255 }));
        let image = ColorImage::from_gray_iter([b.width_px as usize, b.height_px as usize], pixels);
        let texture = ctx.load_texture("label-preview", image, TextureOptions::LINEAR);
        self.rendered = Some((key, r, texture));
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

        let selected = self.selected_size().map(Size::title).unwrap_or_default();
        let titles: Vec<String> = self.sizes.items.iter().map(Size::title).collect();
        if let Some(choice) = list_picker(ui, "size", t.size, t.edit_sizes, self.sizes.path(), &selected, &titles) {
            self.state.size = Some(choice);
        }
        if let Some(e) = &self.sizes.error {
            ui.colored_label(ui.visuals().warn_fg_color, format!("{} {e}", t.sizes_error));
        }
        ui.add_space(8.0);

        let Some(template) = self.selected_template().cloned() else { return };
        let names: Vec<String> = self.templates.items.iter().map(|t| t.name.clone()).collect();
        if let Some(choice) =
            list_picker(ui, "template", t.template, t.edit_templates, self.templates.path(), &template.name, &names)
        {
            self.state.template = Some(choice);
        }
        if let Some(e) = &self.templates.error {
            ui.colored_label(ui.visuals().warn_fg_color, format!("{} {e}", t.templates_error));
        }
        ui.add_space(8.0);

        ui.label(t.orientation);
        let mut orientation = self.orientation(&template);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut orientation, Orientation::Portrait, t.portrait);
            ui.selectable_value(&mut orientation, Orientation::Landscape, t.landscape);
        });
        if orientation != self.orientation(&template) {
            self.state.templates.entry(template.name.clone()).or_default().orientation = Some(orientation);
        }
        ui.add_space(8.0);

        let sizes_pt = self.rendered.as_ref().map(|(_, r, _)| r.sizes_pt.clone()).unwrap_or_default();
        for (i, field) in template.fields.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(&field.name);
                if let Some(Some(pt)) = sizes_pt.get(i) {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.weak(format!("{pt:.0} pt"));
                    });
                }
            });
            let text = self
                .state
                .templates
                .entry(template.name.clone())
                .or_default()
                .fields
                .entry(field.name.clone())
                .or_default();
            ui.add(TextEdit::singleline(text).desired_width(f32::INFINITY));
        }
        ui.add_space(12.0);

        let has_text = self.texts(&template).iter().any(|s| !s.trim().is_empty());
        let ready = self.snapshot.health == Some(Health::Ready) && !self.snapshot.busy_printing;
        let can_print = ready && has_text && self.rendered.is_some();
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

/// Heading with an "edit file" button, then a dropdown; returns the newly picked item.
fn list_picker(
    ui: &mut Ui,
    id: &str,
    heading: &str,
    edit: &str,
    file: Option<&Path>,
    selected: &str,
    items: &[String],
) -> Option<String> {
    ui.horizontal(|ui| {
        ui.label(heading);
        if let Some(path) = file {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button(edit).clicked() {
                    open_in_editor(path);
                }
            });
        }
    });
    let mut picked = None;
    ComboBox::from_id_salt(id).width(ui.available_width()).selected_text(selected).show_ui(ui, |ui| {
        for item in items {
            if ui.selectable_label(item == selected, item).clicked() {
                picked = Some(item.clone());
            }
        }
    });
    picked
}

fn open_in_editor(path: &Path) {
    let _ = if cfg!(target_os = "macos") {
        Command::new("open").arg("-t").arg(path).spawn()
    } else if cfg!(windows) {
        Command::new("notepad").arg(path).spawn()
    } else {
        Command::new("xdg-open").arg(path).spawn()
    };
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
        self.sizes.poll();
        self.templates.poll();
        self.rerender_if_needed(ctx);
        self.save_state_if_settled();
        // Keep polling the config files and flushing state even when nothing else repaints.
        ctx.request_repaint_after(Duration::from_secs(1));
        #[cfg(debug_assertions)]
        self.screenshot.tick(ctx);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let t = tr(self.lang());
        Panel::left("controls").exact_size(320.0).show(ui, |ui| {
            ui.add_space(8.0);
            egui::ScrollArea::vertical().show(ui, |ui| self.controls(ui, t));
        });
        CentralPanel::default().show(ui, |ui| self.preview(ui));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.state.save(&self.dir);
    }
}
