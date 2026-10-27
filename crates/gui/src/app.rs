use std::{collections::BTreeMap, ops::Range, path::Path, process::Command};

use eframe::egui::{
    self, Align, Button, CentralPanel, Color32, ColorImage, ComboBox, Layout, Panel, RichText, Stroke, TextEdit,
    TextureHandle, TextureOptions, Ui, UiBuilder, Vec2, ViewportCommand,
};
use web_time::{Duration, Instant};
use itmo_labels_core::{
    fonts,
    label::Bitmap,
    render::{self, LabelSpec, Orientation as RenderOrientation, Rendered, TextField, Zone},
    tspl::Tspl,
};

use crate::{
    batch::{Batch, ONE_BY_ONE, Outcome, Reason},
    config::{self, Lang, Orientation, Size, Source, State, Template},
    editor::{self, Editor},
    i18n::{Tr, tr},
    printer::{Health, Printers, Snapshot},
    ranges::{self, Mode, Series},
};

pub const DPI: u32 = 203;
const SAVE_DELAY: Duration = Duration::from_secs(1);
/// Narrower than this, the preview goes under the controls instead of beside them.
const NARROW: f32 = 700.0;
/// A longer series asks before printing.
const CONFIRM_ABOVE: u64 = 50;
/// Under the preview of a series: the label number and arrows.
const STEPPER: f32 = 36.0;

pub struct Fonts {
    regular: fonts::Font<'static>,
    bold: fonts::Font<'static>,
}

impl Fonts {
    pub fn embedded() -> Self {
        Self { regular: fonts::regular(), bold: fonts::bold() }
    }
}

/// Everything that changes the rendered label; a change triggers a re-render.
#[derive(PartialEq)]
pub struct RenderKey {
    pub size: Size,
    pub template: Template,
    pub orientation: Orientation,
    /// One per field.
    pub texts: Vec<String>,
}

impl RenderKey {
    pub fn render(&self, fonts: &Fonts) -> Rendered {
        self.render_with(&self.texts, fonts)
    }

    /// The same label with other texts, one per field.
    fn render_with(&self, texts: &[String], fonts: &Fonts) -> Rendered {
        let fields: Vec<TextField> = self
            .template
            .fields
            .iter()
            .zip(texts)
            .map(|(f, text)| TextField {
                text,
                font: if f.bold { &fonts.bold } else { &fonts.regular },
                zone: Zone { x: f.x, y: f.y, w: f.w, h: f.h },
                align: f.align.into(),
                fit: f.fit.into(),
                max_pt: f.max_pt,
                wrap: f.wrap,
            })
            .collect();
        render::render(&LabelSpec {
            width_mm: self.size.width_mm,
            height_mm: self.size.height_mm,
            gap_mm: self.size.gap_mm,
            dpi: DPI,
            orientation: match self.orientation {
                Orientation::Portrait => RenderOrientation::Portrait,
                Orientation::Landscape => RenderOrientation::Landscape,
            },
            fields: &fields,
        })
    }
}

pub fn texture(ctx: &egui::Context, name: &str, b: &Bitmap) -> TextureHandle {
    let pixels = (0..b.height_px).flat_map(|y| (0..b.width_px).map(move |x| if b.get(x, y) { 0 } else { 255 }));
    let image = ColorImage::from_gray_iter([b.width_px as usize, b.height_px as usize], pixels);
    ctx.load_texture(name, image, TextureOptions::LINEAR)
}

/// The label centred on a grey backdrop filling `area`; returns where the label landed.
pub fn paint_label(ui: &Ui, area: egui::Rect, texture: &TextureHandle, size_px: Vec2) -> egui::Rect {
    ui.painter().rect_filled(area, 0.0, Color32::from_gray(200));
    let avail = area.size() - Vec2::splat(48.0);
    let scale = (avail.x / size_px.x).min(avail.y / size_px.y).max(0.05);
    let rect = egui::Rect::from_center_size(area.center(), size_px * scale);
    ui.painter().rect_filled(rect.translate(Vec2::new(2.0, 3.0)), 3.0, Color32::from_black_alpha(50));
    egui::Image::new(texture).corner_radius(3.0).paint_at(ui, rect);
    ui.painter().rect_stroke(rect, 3.0, Stroke::new(1.0, Color32::from_gray(150)), egui::StrokeKind::Outside);
    rect
}

/// A series being printed, fixed when it started: later edits do not change it.
struct Run {
    batch: Batch,
    /// Texts as typed, placeholders included.
    key: RenderKey,
    series: Series,
}

impl Run {
    /// One job with these labels of the series, back to back.
    fn job(&self, labels: Range<u64>, fonts: &Fonts) -> Result<Vec<u8>, String> {
        let mut job = Vec::new();
        for i in labels {
            let r = self.key.render_with(&self.series.texts(&self.key.texts, i), fonts);
            job.extend((Tspl { dpi: DPI }).encode(&r.label, 1).map_err(|e| e.to_string())?);
        }
        Ok(job)
    }
}

/// A user-editable list that keeps its last valid contents while the file is broken.
struct Watched<T> {
    items: Vec<T>,
    file: Option<Source<Vec<T>>>,
    error: Option<String>,
}

impl<T> Watched<T> {
    fn new(builtin: Vec<T>, file: std::io::Result<Source<Vec<T>>>) -> Self {
        match file {
            Ok(file) => Self { items: builtin, file: Some(file), error: None },
            Err(e) => Self { items: builtin, file: None, error: Some(e.to_string()) },
        }
    }

    fn poll(&mut self) {
        match self.file.as_mut().and_then(Source::poll) {
            Some(Ok(items)) => {
                self.items = items;
                self.error = None;
            }
            Some(Err(e)) => self.error = Some(e),
            None => {}
        }
    }

    /// Where to open the file in a text editor; never in the web build.
    fn path(&self) -> Option<&Path> {
        self.file.as_ref().and_then(Source::path)
    }
}

pub struct App {
    state: State,
    saved: State,
    changed_at: Instant,
    sizes: Watched<Size>,
    templates: Watched<Template>,
    printers: Printers,
    snapshot: Snapshot,
    rendered: Option<(RenderKey, Rendered, TextureHandle)>,
    /// What the current texts and ranges make; `None` while a range is invalid.
    series: Option<Series>,
    /// Which label of the series the preview shows.
    shown: u64,
    run: Option<Run>,
    /// A long series waiting for "yes": its length.
    confirm: Option<u64>,
    fonts: Fonts,
    editor: Editor,
    #[cfg(not(target_arch = "wasm32"))]
    serving: crate::serving::Serving,
    #[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
    screenshot: crate::debug::Screenshot,
}

impl App {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new(ctx: &egui::Context) -> Self {
        let dir = config::dir();
        let state = State::load();
        let wake = ctx.clone();
        let service = crate::printer::Service::spawn(move || wake.request_repaint());
        let wake = ctx.clone();
        let printers = Printers::new(service.clone(), state.printer.clone(), move || wake.request_repaint());
        Self {
            sizes: Watched::new(config::builtin_sizes(), config::sizes_file(&dir)),
            templates: Watched::new(config::builtin_templates(), config::templates_file(&dir)),
            serving: crate::serving::Serving::new(service, &state),
            saved: state.clone(),
            state,
            changed_at: Instant::now(),
            printers,
            snapshot: Snapshot::default(),
            rendered: None,
            series: None,
            shown: 0,
            run: None,
            confirm: None,
            fonts: Fonts::embedded(),
            editor: Editor::default(),
            #[cfg(debug_assertions)]
            screenshot: crate::debug::Screenshot::from_env(),
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn new(ctx: &egui::Context) -> Self {
        let state = State::load();
        Self {
            sizes: Watched::new(config::builtin_sizes(), crate::web::sizes_file(ctx)),
            templates: Watched::new(config::builtin_templates(), crate::web::templates_file(ctx)),
            printers: Printers::new(ctx, state.printer.clone()),
            saved: state.clone(),
            state,
            changed_at: Instant::now(),
            snapshot: Snapshot::default(),
            rendered: None,
            series: None,
            shown: 0,
            run: None,
            confirm: None,
            fonts: Fonts::embedded(),
            editor: Editor::default(),
        }
    }

    fn lang(&self) -> Lang {
        self.state.lang.unwrap_or_else(Lang::from_system)
    }

    fn selected_size(&self) -> Option<&Size> {
        let wanted = self.state.size.as_deref();
        self.sizes.items.iter().find(|s| Some(s.title().as_str()) == wanted).or(self.sizes.items.first())
    }

    /// While the editor is open the main window works on its draft, so changes show up live.
    fn template_list(&self) -> &[Template] {
        if self.editor.is_open() { self.editor.live() } else { &self.templates.items }
    }

    fn selected_template(&self) -> Option<&Template> {
        let wanted = self.state.template.as_deref();
        let list = self.template_list();
        list.iter().find(|t| Some(t.name.as_str()) == wanted).or(list.first())
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

    /// With the texts as typed, placeholders included.
    fn base_key(&self) -> Option<RenderKey> {
        let template = self.selected_template()?.clone();
        Some(RenderKey {
            size: self.selected_size()?.clone(),
            orientation: self.orientation(&template),
            texts: self.texts(&template),
            template,
        })
    }

    /// The label on show: number `shown` of the series, or the texts as typed while a range is invalid.
    fn render_key(&self) -> Option<RenderKey> {
        let mut key = self.base_key()?;
        if let Some(s) = &self.series {
            key.texts = s.texts(&key.texts, self.shown);
        }
        Some(key)
    }

    fn make_series(&self, template: &Template) -> Option<Series> {
        let texts = self.texts(template);
        let names = ranges::names(texts.iter().map(String::as_str));
        let saved = self.state.templates.get(&template.name);
        let none = BTreeMap::new();
        Series::new(&names, saved.map_or(&none, |s| &s.ranges), saved.map(|s| s.mode).unwrap_or_default())
    }

    /// Back to the first label whenever the series changes.
    fn follow_series(&mut self) {
        let series = self.selected_template().and_then(|t| self.make_series(t));
        if series != self.series {
            self.shown = 0;
            self.series = series;
        }
    }

    fn start_run(&mut self) {
        let (Some(key), Some(series)) = (self.base_key(), self.series.clone()) else { return };
        let (batch, first) = Batch::start(series.count);
        let mut run = Run { batch, key, series };
        match run.job(first, &self.fonts) {
            Ok(job) => self.printers.print(job),
            Err(e) => run.batch.fail(e),
        }
        self.run = Some(run);
    }

    fn advance_run(&mut self) {
        let Some(run) = &mut self.run else { return };
        let Some(labels) = run.batch.tick(&self.snapshot) else { return };
        match run.job(labels, &self.fonts) {
            Ok(job) => self.printers.print(job),
            Err(e) => run.batch.fail(e),
        }
    }

    fn running(&self) -> bool {
        self.run.as_ref().is_some_and(|r| r.batch.outcome().is_none())
    }

    fn rerender_if_needed(&mut self, ctx: &egui::Context) {
        let Some(key) = self.render_key() else {
            self.rendered = None;
            return;
        };
        if self.rendered.as_ref().is_some_and(|(k, ..)| *k == key) {
            return;
        }
        let r = key.render(&self.fonts);
        let texture = texture(ctx, "label-preview", &r.preview);
        self.rendered = Some((key, r, texture));
    }

    fn save_state_if_settled(&mut self) {
        if self.state != self.saved {
            if self.changed_at.elapsed() >= SAVE_DELAY {
                let _ = self.state.save();
                self.saved = self.state.clone();
            }
        } else {
            self.changed_at = Instant::now();
        }
    }

    fn open_editor(&mut self, ctx: &egui::Context) {
        if self.editor.is_open() {
            ctx.send_viewport_cmd_to(editor::viewport_id(), ViewportCommand::Focus);
            return;
        }
        let template = self.selected_template().map(|t| t.name.clone()).unwrap_or_default();
        let orientation = self.selected_template().map(|t| self.orientation(t)).unwrap_or_default();
        let size = self.selected_size().map(Size::title).unwrap_or_default();
        let writer = self.templates.file.as_ref().map(Source::<Vec<Template>>::writer);
        self.editor.open(&self.templates.items, writer, &template, &size, orientation);
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
                    self.printers.select(Some(name.clone()));
                }
            }
        });
        if current.is_some() {
            status_line(ui, t, &self.snapshot);
        }
        ui.add_space(8.0);

        let selected = self.selected_size().map(Size::title).unwrap_or_default();
        let titles: Vec<String> = self.sizes.items.iter().map(Size::title).collect();
        let (choice, edit) =
            list_picker(ui, "size", t.size, t.edit_sizes, self.sizes.path().is_some(), &selected, &titles);
        if let Some(choice) = choice {
            self.state.size = Some(choice);
        }
        if edit && let Some(path) = self.sizes.path() {
            open_in_editor(path);
        }
        if let Some(e) = &self.sizes.error {
            ui.colored_label(ui.visuals().warn_fg_color, format!("{} {e}", t.sizes_error));
        }
        ui.add_space(8.0);

        let template = self.selected_template().cloned();
        let names: Vec<String> = self.template_list().iter().map(|t| t.name.clone()).collect();
        let shown = template.as_ref().map_or(t.no_templates, |t| t.name.as_str());
        let editable = self.templates.file.is_some();
        let (choice, edit) = list_picker(ui, "template", t.template, t.edit_templates, editable, shown, &names);
        if let Some(choice) = choice {
            self.state.template = Some(choice);
        }
        if edit {
            self.open_editor(ui.ctx());
        }
        if let Some(e) = &self.templates.error {
            ui.colored_label(ui.visuals().warn_fg_color, format!("{} {e}", t.templates_error));
        }
        ui.add_space(8.0);
        let Some(template) = template else { return };

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
            ui.add(TextEdit::singleline(text).hint_text(field.sample.clone()).desired_width(f32::INFINITY));
        }
        let names = ranges::names(self.texts(&template).iter().map(String::as_str));
        if !names.is_empty() {
            self.range_rows(ui, t, &template.name, &names);
        }
        ui.add_space(12.0);

        let has_text = self.texts(&template).iter().any(|s| !s.trim().is_empty());
        let running = self.running();
        let count = self.series.as_ref().map(|s| s.count);
        let ready = self.snapshot.health == Some(Health::Ready) && !self.snapshot.busy_printing && !running;
        let can_print = ready && has_text && self.rendered.is_some() && count.is_some();
        let current = self.run.as_ref().and_then(|r| r.batch.current().map(|labels| (labels, r.batch.total)));
        let label = match (current, count) {
            (Some((labels, total)), _) if labels.end - labels.start > 1 => {
                format!("{} {}–{} / {total}…", t.print, labels.start + 1, labels.end)
            }
            (Some((labels, total)), _) => format!("{} {} / {total}…", t.print, labels.end),
            _ if self.snapshot.busy_printing => t.printing.to_owned(),
            (_, Some(n)) if n > 1 => format!("{} {n}", t.print),
            _ => t.print.to_owned(),
        };
        let print = Button::new(RichText::new(label).size(18.0));
        let clicked = if running {
            let can_stop = self.run.as_ref().is_some_and(|r| r.batch.can_stop());
            let stop = Button::new(RichText::new(t.stop).size(18.0)).min_size(Vec2::new(80.0, 40.0));
            let width = ui.available_width() - 80.0 - ui.spacing().item_spacing.x;
            ui.horizontal(|ui| {
                ui.add_enabled(false, print.min_size(Vec2::new(width, 40.0)));
                if ui.add_enabled(can_stop, stop).clicked()
                    && let Some(run) = &mut self.run
                {
                    run.batch.stop();
                }
            });
            false
        } else {
            ui.add_enabled(can_print, print.min_size(Vec2::new(ui.available_width(), 40.0))).clicked()
        };
        if clicked {
            match count {
                Some(n) if n > CONFIRM_ABOVE => self.confirm = Some(n),
                Some(n) if n > 1 => self.start_run(),
                _ => {
                    self.run = None;
                    if let Some((_, r, _)) = &self.rendered
                        && let Ok(job) = (Tspl { dpi: DPI }).encode(&r.label, 1)
                    {
                        self.printers.print(job);
                    }
                }
            }
        }
        let error = ui.visuals().error_fg_color;
        match self.run.as_ref().map(|r| (&r.batch, r.batch.outcome())) {
            Some((_, None)) => {}
            Some((b, Some(Outcome::Done))) => {
                ui.label(format!("{}: {}", t.printed, b.total));
            }
            Some((b, Some(Outcome::Stopped))) => {
                ui.label(format!("{} {} {} {}", t.printed_n, b.printed, t.of, b.total));
            }
            Some((b, Some(Outcome::Failed(reason)))) => {
                let reason = match reason {
                    Reason::Printer(h) => health_text(t, h).0,
                    Reason::Job(e) => e.clone(),
                };
                ui.colored_label(error, format!("{} {} {} {}: {reason}", t.printed_n, b.printed, t.of, b.total));
            }
            Some((_, Some(Outcome::RestFailed(e)))) => {
                ui.colored_label(error, format!("{} {ONE_BY_ONE} {} {e}", t.first, t.rest_failed));
            }
            None => match &self.snapshot.last_job {
                Some(Ok(())) => {
                    ui.label(t.printed);
                }
                Some(Err(e)) => {
                    ui.colored_label(error, format!("{} {e}", t.print_failed));
                }
                None => {}
            },
        }
    }

    /// Start, stop and step for every placeholder name, and how they combine.
    fn range_rows(&mut self, ui: &mut Ui, t: &Tr, template: &str, names: &[String]) {
        let count = self.series.as_ref().map(|s| s.count);
        let saved = self.state.templates.entry(template.to_owned()).or_default();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(t.ranges);
            if names.len() >= 2
                && let Some(n) = count
            {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.weak(format!("{} {n}", t.total));
                });
            }
        });
        if names.len() >= 2 {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut saved.mode, Mode::Product, t.product).on_hover_text(t.product_hint);
                ui.selectable_value(&mut saved.mode, Mode::Zip, t.zip).on_hover_text(t.zip_hint);
            });
        }
        for name in names {
            let range = saved.ranges.entry(name.clone()).or_default();
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{{{name}}}")).strong());
                // Room for the three words and the gaps around six widgets.
                let words = 60.0 + 6.0 * ui.spacing().item_spacing.x;
                let w = ((ui.available_width() - words) / 3.0).max(32.0);
                for (word, value) in [(t.from, &mut range.start), (t.to, &mut range.stop), (t.step, &mut range.step)] {
                    ui.label(word);
                    ui.add(TextEdit::singleline(value).desired_width(w));
                }
            });
            if let Some(e) = range.error() {
                let text = match e {
                    ranges::RangeError::Start => t.bad_start,
                    ranges::RangeError::Stop => t.bad_stop,
                    ranges::RangeError::Step => t.bad_step,
                    ranges::RangeError::ZeroStep => t.zero_step,
                    ranges::RangeError::Direction => t.wrong_direction,
                };
                ui.colored_label(ui.visuals().error_fg_color, text);
            }
        }
    }

    fn confirm_question(&mut self, ctx: &egui::Context, t: &Tr) {
        let Some(n) = self.confirm else { return };
        let mut answer = None;
        let modal = egui::Modal::new(egui::Id::new("confirm-series")).show(ctx, |ui| {
            ui.label(RichText::new(format!("{} {n} {}?", t.confirm_print, t.labels(n))).strong().size(15.0));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(t.print).clicked() {
                    answer = Some(true);
                }
                if ui.button(t.cancel).clicked() {
                    answer = Some(false);
                }
            });
        });
        if modal.should_close() {
            answer.get_or_insert(false);
        }
        if let Some(go) = answer {
            self.confirm = None;
            if go {
                self.start_run();
            }
        }
    }

    fn count(&self) -> u64 {
        self.series.as_ref().map_or(1, |s| s.count)
    }

    /// Fills `area`, or with `None` the rest of the panel; a series gets arrows under the label.
    fn preview(&mut self, ui: &mut Ui, area: Option<egui::Rect>) {
        let Some((_, r, texture)) = &self.rendered else { return };
        let size = Vec2::new(r.preview.width_px as f32, r.preview.height_px as f32);
        let area = area.unwrap_or_else(|| ui.available_rect_before_wrap());
        let count = self.count();
        if count <= 1 {
            paint_label(ui, area, texture, size);
            return;
        }
        ui.painter().rect_filled(area, 0.0, Color32::from_gray(200));
        paint_label(ui, area.with_max_y(area.max.y - STEPPER), texture, size);
        let bar = egui::Rect::from_center_size(
            egui::pos2(area.center().x, area.max.y - STEPPER / 2.0 - 4.0),
            Vec2::new(200.0, 28.0),
        );
        let mut shown = self.shown;
        ui.scope_builder(UiBuilder::new().max_rect(bar).layout(Layout::left_to_right(Align::Center)), |ui| {
            if ui.add_enabled(shown > 0, Button::new(RichText::new("‹").size(20.0)).min_size(Vec2::splat(28.0))).clicked() {
                shown -= 1;
            }
            let number = RichText::new(format!("{} / {count}", shown + 1)).color(Color32::from_gray(40));
            ui.add_sized([200.0 - 2.0 * 28.0 - 2.0 * ui.spacing().item_spacing.x, 28.0], egui::Label::new(number));
            if ui.add_enabled(shown + 1 < count, Button::new(RichText::new("›").size(20.0)).min_size(Vec2::splat(28.0))).clicked() {
                shown += 1;
            }
        });
        self.shown = shown;
    }

    /// One column: the controls, then the preview as tall as the label needs.
    fn narrow_layout(&mut self, ui: &mut Ui, t: &Tr) {
        CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(8.0);
                self.side_panel(ui, t);
                ui.add_space(12.0);
                if let Some((_, r, _)) = &self.rendered {
                    let w = ui.available_width();
                    let stepper = if self.count() > 1 { STEPPER } else { 0.0 };
                    let h = (w * r.preview.height_px as f32 / r.preview.width_px as f32 + 48.0).clamp(160.0, 560.0)
                        + stepper;
                    let (area, _) = ui.allocate_exact_size(Vec2::new(w, h), egui::Sense::hover());
                    self.preview(ui, Some(area));
                }
            });
        });
    }

    fn side_panel(&mut self, ui: &mut Ui, t: &Tr) {
        self.controls(ui, t);
        #[cfg(not(target_arch = "wasm32"))]
        self.serving.controls(ui, t, &mut self.state);
    }

    /// Called from `logic`, not `ui`: eframe skips `ui` while every window is covered
    /// (e.g. after Cmd+Tab), and a viewport left out of a frame gets destroyed, together
    /// with the GL surface glow may still have current, which panics on the next paint.
    #[cfg(not(target_arch = "wasm32"))]
    fn show_editor(&mut self, ctx: &egui::Context) {
        let t = tr(self.lang());
        let builder = egui::ViewportBuilder::default()
            .with_title(format!("{} — ITMO Labels", t.editor_title))
            .with_inner_size([1100.0, 680.0])
            .with_min_inner_size([820.0, 500.0]);
        ctx.clone().show_viewport_immediate(editor::viewport_id(), builder, |ui, _| self.editor_frame(ui));
    }

    fn editor_frame(&mut self, ui: &mut Ui) {
        let t = tr(self.lang());
        let outcome = self.editor.show(ui, t, &self.fonts, &self.sizes.items);
        for (old, new) in outcome.renames {
            if self.state.template.as_ref() == Some(&old) {
                self.state.template = Some(new);
            }
        }
        if let Some(saved) = outcome.saved {
            self.templates.items = saved;
        }
    }
}

/// Heading with an "edit" button (when there is a file to edit), then a dropdown;
/// returns the newly picked item and whether "edit" was clicked.
fn list_picker(
    ui: &mut Ui,
    id: &str,
    heading: &str,
    edit: &str,
    editable: bool,
    selected: &str,
    items: &[String],
) -> (Option<String>, bool) {
    let mut edit_clicked = false;
    ui.horizontal(|ui| {
        ui.label(heading);
        if editable {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                edit_clicked = ui.small_button(edit).clicked();
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
    (picked, edit_clicked)
}

pub fn open_in_editor(path: &Path) {
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
        Some(h) => health_text(t, h),
    };
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 5.0, color);
        ui.label(text);
    });
}

fn health_text(t: &Tr, h: &Health) -> (String, Color32) {
    match h {
        Health::Ready => (t.ready.to_owned(), Color32::from_rgb(0x2e, 0xa0, 0x43)),
        Health::Printing => (t.busy_printing.to_owned(), Color32::from_rgb(0x2f, 0x81, 0xf7)),
        Health::NoPaper => (t.no_paper.to_owned(), Color32::from_rgb(0xd2, 0x99, 0x22)),
        Health::HeadOpen => (t.head_open.to_owned(), Color32::from_rgb(0xd2, 0x99, 0x22)),
        Health::PaperJam => (t.paper_jam.to_owned(), Color32::from_rgb(0xd2, 0x99, 0x22)),
        Health::Paused => (t.paused.to_owned(), Color32::from_rgb(0xd2, 0x99, 0x22)),
        Health::Error(b) => (format!("{} ({b:#04x})", t.error), Color32::from_rgb(0xda, 0x36, 0x33)),
        Health::NoReply => (t.no_reply.to_owned(), Color32::from_rgb(0xda, 0x36, 0x33)),
        Health::Busy => (t.busy.to_owned(), Color32::from_rgb(0xd2, 0x99, 0x22)),
        Health::Unavailable(e) => (format!("{}: {e}", t.unavailable), Color32::from_rgb(0xda, 0x36, 0x33)),
        Health::Disconnected => (t.disconnected.to_owned(), Color32::GRAY),
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.snapshot = self.printers.snapshot();
        self.advance_run();
        if self.state.printer.is_none()
            && let Some(first) = self.snapshot.printers.first()
        {
            self.state.printer = Some(first.clone());
            self.printers.select(Some(first.clone()));
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.serving.sync(&mut self.state);
        self.sizes.poll();
        self.templates.poll();
        // An unsaved draft wins over edits to the file; without one the editor follows the file.
        self.editor.file_changed(&self.templates.items);
        if ctx.input(|i| i.viewport().close_requested()) && self.editor.is_open() && self.editor.dirty() {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd_to(editor::viewport_id(), ViewportCommand::Focus);
            self.editor.ask_before_quitting();
        }
        // glow cannot screenshot a separate window, so the editor is drawn inside the main one.
        #[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
        if self.screenshot.wants_editor() && !self.editor.is_open() {
            ctx.set_embed_viewports(true);
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(1400.0, 860.0)));
            self.open_editor(ctx);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if self.editor.is_open() {
            self.show_editor(ctx);
        }
        self.follow_series();
        self.rerender_if_needed(ctx);
        self.save_state_if_settled();
        // Keep polling the config files and flushing state even when nothing else repaints.
        ctx.request_repaint_after(Duration::from_secs(1));
        #[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
        self.screenshot.tick(ctx);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let t = tr(self.lang());
        // A web page has no second window: the editor takes the whole page instead.
        if cfg!(target_arch = "wasm32") && self.editor.is_open() {
            self.editor_frame(ui);
            return;
        }
        self.confirm_question(&ui.ctx().clone(), t);
        if ui.available_width() < NARROW {
            self.narrow_layout(ui, t);
            return;
        }
        Panel::left("controls").exact_size(320.0).show(ui, |ui| {
            ui.add_space(8.0);
            egui::ScrollArea::vertical().show(ui, |ui| self.side_panel(ui, t));
        });
        CentralPanel::default().show(ui, |ui| self.preview(ui, None));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.state.save();
    }
}
