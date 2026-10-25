//! Template editor: a window of its own that edits a draft of the whole `templates.toml`.
//! The main window renders the draft as it changes; the file is only written on Save.

use std::{collections::HashMap, mem};

use eframe::egui::{
    self, Align2, Button, CentralPanel, Color32, ComboBox, CursorIcon, DragValue, FontId, Grid, Id, Key,
    Modifiers, Panel, Pos2, Rect, Response, RichText, ScrollArea, Sense, Stroke, StrokeKind,
    TextEdit, TextureHandle, Ui, Vec2, ViewportCommand, ViewportId,
};
use itmo_labels_core::render::{MARGIN_MM, Rendered};

use crate::{
    app::{self, DPI, Fonts, RenderKey},
    config::{Field, FieldAlign, FieldFit, Orientation, Size, Template, TemplatesWriter},
    i18n::Tr,
};

/// Zones snap to multiples of this when no other edge is near.
const GRID: f32 = 0.01;
/// Smallest zone side, as a fraction; also the smallest field drawn on the canvas.
const MIN_SIDE: f32 = 0.02;
/// How close, in screen points, an edge has to come to another one to stick to it.
const SNAP_PX: f32 = 6.0;
const UNDO_LIMIT: usize = 100;
/// Arrow-key step, and with Shift.
const NUDGE: f32 = 0.01;
const NUDGE_FAR: f32 = 0.1;
const ACCENT: Color32 = Color32::from_rgb(0x2f, 0x81, 0xf7);

pub fn viewport_id() -> ViewportId {
    ViewportId::from_hash_of("template-editor")
}

/// A template plus an id that survives renames and reordering, so the main window can
/// keep its selection when the selected template is renamed.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    id: u64,
    template: Template,
}

/// A zone by its edges, in fractions of the printable area.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Edges {
    l: f32,
    t: f32,
    r: f32,
    b: f32,
}

impl Edges {
    fn of(f: &Field) -> Self {
        Self { l: f.x, t: f.y, r: f.x + f.w, b: f.y + f.h }
    }

    fn apply(self, f: &mut Field) {
        f.x = self.l;
        f.y = self.t;
        f.w = self.r - self.l;
        f.h = self.b - self.t;
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Grab {
    Move,
    /// Which edges follow the pointer.
    Resize { l: bool, t: bool, r: bool, b: bool },
}

/// Edges of the other zones and of the area itself, for snapping.
struct Snap {
    xs: Vec<f32>,
    ys: Vec<f32>,
    /// `SNAP_PX` in fractions along each axis.
    tol: Vec2,
}

impl Snap {
    fn new(others: impl Iterator<Item = Edges>, tol: Vec2) -> Self {
        let (mut xs, mut ys) = (vec![0.0, 1.0], vec![0.0, 1.0]);
        for e in others {
            xs.extend([e.l, e.r]);
            ys.extend([e.t, e.b]);
        }
        Self { xs, ys, tol }
    }
}

fn to_grid(v: f32) -> f32 {
    (v / GRID).round() * GRID
}

/// The nearest target within `tol`, else the nearest grid line.
fn snap_point(v: f32, targets: &[f32], tol: f32) -> f32 {
    targets
        .iter()
        .copied()
        .filter(|t| (t - v).abs() <= tol)
        .min_by(|a, b| (a - v).abs().total_cmp(&(b - v).abs()))
        .unwrap_or_else(|| to_grid(v))
}

/// How far to shift the span `a..b` so that its end nearer to a target lands on it,
/// else so that `a` lands on the grid.
fn snap_shift(a: f32, b: f32, targets: &[f32], tol: f32) -> f32 {
    let near = |v: f32| targets.iter().map(|t| t - v).filter(|d| d.abs() <= tol).min_by(|x, y| x.abs().total_cmp(&y.abs()));
    match (near(a), near(b)) {
        (Some(x), Some(y)) => {
            if x.abs() <= y.abs() {
                x
            } else {
                y
            }
        }
        (Some(d), None) | (None, Some(d)) => d,
        (None, None) => to_grid(a) - a,
    }
}

/// Where a zone ends up when `grab` is dragged by `d`: snapped (unless `snap` is `None`),
/// at least `MIN_SIDE` on each side and inside the area.
fn dragged(start: Edges, grab: Grab, d: Vec2, snap: Option<&Snap>) -> Edges {
    let point = |v: f32, targets: &[f32], tol: f32| match snap {
        Some(_) => snap_point(v, targets, tol),
        None => v,
    };
    let (xs, ys, tol) = snap.map_or((&[][..], &[][..], Vec2::ZERO), |s| (&s.xs[..], &s.ys[..], s.tol));
    match grab {
        Grab::Move => {
            let (w, h) = (start.r - start.l, start.b - start.t);
            let (mut l, mut t) = (start.l + d.x, start.t + d.y);
            if snap.is_some() {
                l += snap_shift(l, l + w, xs, tol.x);
                t += snap_shift(t, t + h, ys, tol.y);
            }
            let (l, t) = (l.clamp(0.0, 1.0 - w), t.clamp(0.0, 1.0 - h));
            Edges { l, t, r: l + w, b: t + h }
        }
        Grab::Resize { l, t, r, b } => {
            let mut e = start;
            if l {
                e.l = point(start.l + d.x, xs, tol.x).clamp(0.0, e.r - MIN_SIDE);
            }
            if r {
                e.r = point(start.r + d.x, xs, tol.x).clamp(e.l + MIN_SIDE, 1.0);
            }
            if t {
                e.t = point(start.t + d.y, ys, tol.y).clamp(0.0, e.b - MIN_SIDE);
            }
            if b {
                e.b = point(start.b + d.y, ys, tol.y).clamp(e.t + MIN_SIDE, 1.0);
            }
            e
        }
    }
}

/// The zone spanned by dragging from `a` to `b` on empty canvas.
fn spanned(a: Pos2, b: Pos2, snap: Option<&Snap>) -> Edges {
    let (fx, fy) = match snap {
        Some(s) => (snap_point(b.x, &s.xs, s.tol.x), snap_point(b.y, &s.ys, s.tol.y)),
        None => (b.x, b.y),
    };
    let (ax, ay) = match snap {
        Some(s) => (snap_point(a.x, &s.xs, s.tol.x), snap_point(a.y, &s.ys, s.tol.y)),
        None => (a.x, a.y),
    };
    let (ax, ay, fx, fy) = (ax.clamp(0.0, 1.0), ay.clamp(0.0, 1.0), fx.clamp(0.0, 1.0), fy.clamp(0.0, 1.0));
    Edges { l: ax.min(fx), t: ay.min(fy), r: ax.max(fx), b: ay.max(fy) }
}

/// The tallest full-width band no zone covers, for a field added with the button.
fn free_band(fields: &[Field]) -> Edges {
    let mut spans: Vec<(f32, f32)> = fields.iter().map(|f| (f.y, f.y + f.h)).collect();
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (mut best, mut y) = ((0.0, 0.0), 0.0f32);
    for (a, b) in spans.into_iter().chain([(1.0, 1.0)]) {
        if a - y > best.1 - best.0 {
            best = (y, a);
        }
        y = y.max(b);
    }
    let (t, b) = if best.1 - best.0 < 0.05 { (0.0, 0.2) } else { best };
    Edges { l: 0.0, t, r: 1.0, b }
}

/// `base`, or `base 2`, `base 3`… whichever is free.
fn unique_name(base: &str, taken: &[&str]) -> String {
    (1..)
        .map(|n| if n == 1 { base.to_owned() } else { format!("{base} {n}") })
        .find(|name| !taken.contains(&name.as_str()))
        .expect("some suffix is free")
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Problem {
    Empty,
    Taken,
}

/// Per name: why it cannot be saved, if it cannot.
fn name_problems<'a>(names: impl Iterator<Item = &'a str> + Clone) -> Vec<Option<Problem>> {
    names
        .clone()
        .map(|name| {
            if name.trim().is_empty() {
                Some(Problem::Empty)
            } else if names.clone().filter(|n| *n == name).count() > 1 {
                Some(Problem::Taken)
            } else {
                None
            }
        })
        .collect()
}

#[derive(Default)]
struct Problems {
    templates: HashMap<u64, Problem>,
    fields: HashMap<(u64, usize), Problem>,
}

impl Problems {
    fn of(draft: &[Entry]) -> Self {
        let mut p = Self::default();
        for (e, problem) in draft.iter().zip(name_problems(draft.iter().map(|e| e.template.name.as_str()))) {
            if let Some(problem) = problem {
                p.templates.insert(e.id, problem);
            }
            let fields = name_problems(e.template.fields.iter().map(|f| f.name.as_str()));
            for (i, problem) in fields.into_iter().enumerate() {
                if let Some(problem) = problem {
                    p.fields.insert((e.id, i), problem);
                }
            }
        }
        p
    }

    fn is_empty(&self) -> bool {
        self.templates.is_empty() && self.fields.is_empty()
    }
}

enum DragKind {
    Zone { field: usize, grab: Grab },
    Create,
}

struct Drag {
    kind: DragKind,
    start: Edges,
    /// Where the pointer went down, in fractions of the area.
    origin: Pos2,
}

/// What the main window has to pick up after a frame of the editor.
pub struct Outcome {
    /// Old and new names of templates renamed this frame.
    pub renames: Vec<(String, String)>,
    /// What was just written to the file.
    pub saved: Option<Vec<Template>>,
}

#[derive(Default)]
pub struct Editor {
    open: bool,
    writer: Option<TemplatesWriter>,
    draft: Vec<Entry>,
    /// What the file holds.
    baseline: Vec<Entry>,
    /// The baseline before the last save, until the server confirms it (web only).
    before_save: Option<Vec<Entry>>,
    /// The draft as of the last undo step.
    committed: Vec<Entry>,
    undo: Vec<Vec<Entry>>,
    redo: Vec<Vec<Entry>>,
    next_id: u64,
    selected: Option<u64>,
    field: Option<usize>,
    /// Preview size, by title.
    size: String,
    orientation: Orientation,
    rendered: Option<(RenderKey, Rendered, TextureHandle)>,
    drag: Option<Drag>,
    /// The unsaved-changes question is up; `true` when the whole app is quitting.
    closing: Option<bool>,
    save_error: Option<String>,
    /// The last draft without errors: what the main window shows.
    live: Vec<Template>,
    live_ids: Vec<u64>,
    /// An input box had focus at the end of the last frame: its Esc must not reach the canvas.
    typing: bool,
    /// The web page has no window frame to close the editor with, so it gets a button.
    close_clicked: bool,
}

impl Editor {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn live(&self) -> &[Template] {
        &self.live
    }

    pub fn dirty(&self) -> bool {
        !self.draft.iter().map(|e| &e.template).eq(self.baseline.iter().map(|e| &e.template))
    }

    pub fn open(&mut self, templates: &[Template], writer: Option<TemplatesWriter>, template: &str, size: &str, orientation: Orientation) {
        self.open = true;
        self.writer = writer;
        self.size = size.to_owned();
        self.orientation = orientation;
        self.reset(templates);
        self.selected = self.draft.iter().find(|e| e.template.name == template).or(self.draft.first()).map(|e| e.id);
    }

    /// Starts over from `templates`, keeping the selection by name.
    fn reset(&mut self, templates: &[Template]) {
        let selected = self.current().map(|i| self.draft[i].template.name.clone());
        self.draft = templates.iter().map(|t| self.entry(t.clone())).collect();
        self.baseline = self.draft.clone();
        self.before_save = None;
        self.committed = self.draft.clone();
        self.undo.clear();
        self.redo.clear();
        self.drag = None;
        self.closing = None;
        self.save_error = None;
        self.live = templates.to_vec();
        self.live_ids = self.draft.iter().map(|e| e.id).collect();
        if let Some(name) = selected {
            self.selected = self.draft.iter().find(|e| e.template.name == name).map(|e| e.id);
        }
        self.fix_selection();
    }

    /// The file was re-read: follow it unless there are unsaved changes, which win.
    pub fn file_changed(&mut self, templates: &[Template]) {
        let same = self.baseline.iter().map(|e| &e.template).eq(templates.iter());
        if self.open && !same && !self.dirty() {
            self.reset(templates);
        }
    }

    pub fn ask_before_quitting(&mut self) {
        self.closing = Some(true);
    }

    fn entry(&mut self, template: Template) -> Entry {
        self.next_id += 1;
        Entry { id: self.next_id, template }
    }

    fn current(&self) -> Option<usize> {
        self.draft.iter().position(|e| Some(e.id) == self.selected)
    }

    fn fix_selection(&mut self) {
        if self.current().is_none() {
            self.selected = self.draft.first().map(|e| e.id);
            self.field = None;
        }
        let fields = self.current().map_or(0, |i| self.draft[i].template.fields.len());
        self.field = self.field.filter(|&f| f < fields);
    }

    fn can_save(&self) -> bool {
        self.writer.is_some() && Problems::of(&self.draft).is_empty()
    }

    fn save(&mut self) -> Option<Vec<Template>> {
        let writer = self.writer.as_ref()?;
        let templates: Vec<Template> = self.draft.iter().map(|e| e.template.clone()).collect();
        match writer.write(&templates) {
            Ok(()) => {
                self.before_save = Some(mem::replace(&mut self.baseline, self.draft.clone()));
                self.save_error = None;
                Some(templates)
            }
            Err(e) => {
                self.save_error = Some(e);
                None
            }
        }
    }

    fn step_back(&mut self) {
        if let Some(prev) = self.undo.pop() {
            self.redo.push(mem::replace(&mut self.draft, prev.clone()));
            self.committed = prev;
            self.fix_selection();
        }
    }

    fn step_forward(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(mem::replace(&mut self.draft, next.clone()));
            self.committed = next;
            self.fix_selection();
        }
    }

    /// One undo step per finished edit: once the pointer is up and no input box has focus.
    fn commit_undo_step(&mut self, ctx: &egui::Context) {
        let busy = ctx.input(|i| i.pointer.any_down()) || ctx.memory(|m| m.focused().is_some());
        if !busy && self.draft != self.committed {
            self.undo.push(mem::replace(&mut self.committed, self.draft.clone()));
            if self.undo.len() > UNDO_LIMIT {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
    }

    /// Keyboard commands; returns what Cmd+S wrote.
    fn shortcuts(&mut self, ctx: &egui::Context) -> Option<Vec<Template>> {
        let key = |modifiers: Modifiers, key: Key| ctx.input_mut(|i| i.consume_key(modifiers, key));
        let mut saved = None;
        if key(Modifiers::COMMAND, Key::S) && self.dirty() && self.can_save() {
            saved = self.save();
        }
        // The rest belongs to an input box while one has focus (it has its own undo, arrows and Esc).
        if self.typing || ctx.memory(|m| m.focused().is_some()) || self.drag.is_some() {
            return saved;
        }
        // Checked from the most specific: extra Shift and Alt do not stop a key from matching.
        if key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z) {
            self.step_forward();
        } else if key(Modifiers::COMMAND, Key::Z) {
            self.step_back();
        }
        let Some(ti) = self.current() else { return saved };
        let fields = &mut self.draft[ti].template.fields;

        let prev = key(Modifiers::SHIFT, Key::Tab);
        let next = !prev && key(Modifiers::NONE, Key::Tab);
        if (next || prev) && !fields.is_empty() {
            let n = fields.len();
            self.field = Some(match (self.field, prev) {
                (Some(i), false) => (i + 1) % n,
                (Some(i), true) => (i + n - 1) % n,
                (None, false) => 0,
                (None, true) => n - 1,
            });
        }
        if key(Modifiers::NONE, Key::Escape) {
            self.field = None;
        }
        let Some(fi) = self.field.filter(|&i| i < fields.len()) else {
            Self::keep_focus_still(ctx);
            return saved;
        };
        if key(Modifiers::NONE, Key::Delete) || key(Modifiers::NONE, Key::Backspace) {
            fields.remove(fi);
            self.field = (!fields.is_empty()).then(|| fi.min(fields.len() - 1));
        } else if key(Modifiers::COMMAND, Key::D) {
            let mut copy = fields[fi].clone();
            let taken: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
            copy.name = unique_name(&copy.name, &taken);
            dragged(Edges::of(&copy), Grab::Move, Vec2::splat(2.0 * NUDGE), None).apply(&mut copy);
            fields.insert(fi + 1, copy);
            self.field = Some(fi + 1);
        } else {
            let arrows = [(Key::ArrowLeft, -1.0, 0.0), (Key::ArrowRight, 1.0, 0.0), (Key::ArrowUp, 0.0, -1.0), (Key::ArrowDown, 0.0, 1.0)];
            for (arrow, dx, dy) in arrows {
                let combos = [
                    (Modifiers::ALT | Modifiers::SHIFT, true, NUDGE_FAR),
                    (Modifiers::ALT, true, NUDGE),
                    (Modifiers::SHIFT, false, NUDGE_FAR),
                    (Modifiers::NONE, false, NUDGE),
                ];
                if let Some(&(_, resize, step)) = combos.iter().find(|(m, ..)| key(*m, arrow)) {
                    // Alt moves the right or bottom edge, so the zone grows and shrinks from its top-left corner.
                    let grab = if resize { Grab::Resize { l: false, t: false, r: dx != 0.0, b: dy != 0.0 } } else { Grab::Move };
                    let f = &mut fields[fi];
                    dragged(Edges::of(f), grab, Vec2::new(dx, dy) * step, None).apply(f);
                }
            }
        }
        Self::keep_focus_still(ctx);
        saved
    }

    /// egui also moves keyboard focus with arrows and Tab; on the canvas they mean the zones.
    fn keep_focus_still(ctx: &egui::Context) {
        ctx.memory_mut(|m| m.move_focus(egui::FocusDirection::None));
    }

    pub fn show(&mut self, ui: &mut Ui, t: &Tr, fonts: &Fonts, sizes: &[Size]) -> Outcome {
        let ctx = ui.ctx().clone();
        let before: Vec<(u64, String)> = self.live_ids.iter().copied().zip(self.live.iter().map(|t| t.name.clone())).collect();
        // The server turned down a save `write` had accepted: the draft is unsaved again.
        if let Some(e) = self.writer.as_ref().and_then(TemplatesWriter::failed) {
            if let Some(baseline) = self.before_save.take() {
                self.baseline = baseline;
            }
            self.save_error = Some(e);
        }
        if ctx.input(|i| i.viewport().close_requested()) || mem::take(&mut self.close_clicked) {
            if self.dirty() {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                self.closing.get_or_insert(false);
            } else {
                self.open = false;
            }
        }
        let mut saved = self.shortcuts(&ctx);

        let problems = Problems::of(&self.draft);
        Panel::bottom("editor-actions").show(ui, |ui| self.actions(ui, t, &problems, &mut saved));
        Panel::left("editor-lists").exact_size(210.0).show(ui, |ui| {
            ScrollArea::vertical().show(ui, |ui| self.lists(ui, t, &problems));
        });
        Panel::right("editor-properties").exact_size(280.0).show(ui, |ui| {
            ScrollArea::vertical().show(ui, |ui| self.properties(ui, t, &problems));
        });
        CentralPanel::default().show(ui, |ui| self.canvas(ui, t, fonts, sizes));
        self.closing_question(&ctx, t, &mut saved);

        self.fix_selection();
        self.commit_undo_step(&ctx);
        self.typing = ctx.memory(|m| m.focused().is_some());
        if Problems::of(&self.draft).is_empty() {
            self.live = self.draft.iter().map(|e| e.template.clone()).collect();
            self.live_ids = self.draft.iter().map(|e| e.id).collect();
        }
        let renames = before
            .into_iter()
            .filter_map(|(id, old)| {
                let i = self.live_ids.iter().position(|&l| l == id)?;
                (self.live[i].name != old).then(|| (old, self.live[i].name.clone()))
            })
            .collect();
        Outcome { renames, saved }
    }

    fn actions(&mut self, ui: &mut Ui, t: &Tr, problems: &Problems, saved: &mut Option<Vec<Template>>) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let save = Button::new(RichText::new(t.save).strong()).min_size(Vec2::new(110.0, 0.0));
            if ui.add_enabled(self.dirty() && self.can_save(), save).clicked() {
                *saved = self.save();
            }
            if ui.add_enabled(self.dirty(), Button::new(t.discard)).clicked() {
                self.draft = self.baseline.clone();
            }
            ui.separator();
            if ui.add_enabled(!self.undo.is_empty(), Button::new(t.undo)).clicked() {
                self.step_back();
            }
            if ui.add_enabled(!self.redo.is_empty(), Button::new(t.redo)).clicked() {
                self.step_forward();
            }
            ui.separator();
            if cfg!(target_arch = "wasm32") && ui.button(t.close).clicked() {
                self.close_clicked = true;
            }
            if self.writer.is_none() {
                ui.colored_label(ui.visuals().warn_fg_color, t.no_file);
            } else if !problems.is_empty() {
                ui.colored_label(ui.visuals().error_fg_color, t.fix_errors);
            } else if let Some(e) = &self.save_error {
                ui.colored_label(ui.visuals().error_fg_color, format!("{} {e}", t.save_failed));
            }
            if let Some(path) = self.writer.as_ref().and_then(TemplatesWriter::path) {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.link(t.open_file).clicked() {
                        app::open_in_editor(path);
                    }
                });
            }
        });
        ui.add_space(4.0);
    }

    fn lists(&mut self, ui: &mut Ui, t: &Tr, problems: &Problems) {
        ui.add_space(8.0);
        ui.label(RichText::new(t.templates_heading).strong());
        let current = self.current();
        ui.horizontal_wrapped(|ui| {
            if ui.small_button(t.add).clicked() {
                let taken: Vec<&str> = self.draft.iter().map(|e| e.template.name.as_str()).collect();
                let template = Template {
                    name: unique_name(t.new_template, &taken),
                    orientation: None,
                    fields: vec![Field { name: t.default_field.to_owned(), ..Field::default() }],
                };
                let entry = self.entry(template);
                self.selected = Some(entry.id);
                self.field = Some(0);
                self.draft.insert(current.map_or(self.draft.len(), |i| i + 1), entry);
            }
            if ui.add_enabled(current.is_some(), Button::new(t.duplicate).small()).clicked()
                && let Some(i) = current
            {
                let mut template = self.draft[i].template.clone();
                let taken: Vec<&str> = self.draft.iter().map(|e| e.template.name.as_str()).collect();
                template.name = unique_name(&format!("{} {}", template.name, t.copy_suffix), &taken);
                let entry = self.entry(template);
                self.selected = Some(entry.id);
                self.draft.insert(i + 1, entry);
            }
            if ui.add_enabled(current.is_some(), Button::new(t.delete).small()).clicked()
                && let Some(i) = current
            {
                self.draft.remove(i);
                self.selected = self.draft.get(i).or(self.draft.last()).map(|e| e.id);
                self.field = None;
            }
            if ui.add_enabled(current.is_some_and(|i| i > 0), Button::new(t.move_up).small()).clicked()
                && let Some(i) = current
            {
                self.draft.swap(i, i - 1);
            }
            if ui.add_enabled(current.is_some_and(|i| i + 1 < self.draft.len()), Button::new(t.move_down).small()).clicked()
                && let Some(i) = current
            {
                self.draft.swap(i, i + 1);
            }
        });
        for e in &self.draft {
            let text = list_item(ui, &e.template.name, t, problems.templates.contains_key(&e.id));
            if ui.selectable_label(Some(e.id) == self.selected, text).clicked() && Some(e.id) != self.selected {
                self.selected = Some(e.id);
                self.field = None;
            }
        }

        ui.add_space(8.0);
        ui.separator();
        ui.label(RichText::new(t.fields_heading).strong());
        let Some(ti) = self.current() else { return };
        let id = self.draft[ti].id;
        let fields = &mut self.draft[ti].template.fields;
        let field = self.field;
        ui.horizontal_wrapped(|ui| {
            if ui.small_button(t.add).clicked() {
                let taken: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
                let mut f = Field { name: unique_name(t.new_field, &taken), ..Field::default() };
                free_band(fields).apply(&mut f);
                fields.push(f);
                self.field = Some(fields.len() - 1);
            }
            if ui.add_enabled(field.is_some(), Button::new(t.delete).small()).clicked()
                && let Some(i) = field
            {
                fields.remove(i);
                self.field = None;
            }
            if ui.add_enabled(field.is_some_and(|i| i > 0), Button::new(t.move_up).small()).clicked()
                && let Some(i) = field
            {
                fields.swap(i, i - 1);
                self.field = Some(i - 1);
            }
            if ui.add_enabled(field.is_some_and(|i| i + 1 < fields.len()), Button::new(t.move_down).small()).clicked()
                && let Some(i) = field
            {
                fields.swap(i, i + 1);
                self.field = Some(i + 1);
            }
        });
        for (i, f) in fields.iter().enumerate() {
            let text = list_item(ui, &f.name, t, problems.fields.contains_key(&(id, i)));
            if ui.selectable_label(self.field == Some(i), text).clicked() {
                self.field = Some(i);
            }
        }
    }

    fn properties(&mut self, ui: &mut Ui, t: &Tr, problems: &Problems) {
        ui.add_space(8.0);
        let Some(ti) = self.current() else {
            ui.weak(t.pick_template);
            return;
        };
        let entry = &mut self.draft[ti];
        let template = &mut entry.template;
        ui.label(RichText::new(t.template).strong());
        ui.label(t.name);
        name_input(ui, &mut template.name, problems.templates.get(&entry.id).copied(), t);
        ui.label(t.default_orientation);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut template.orientation, None, t.not_set);
            ui.selectable_value(&mut template.orientation, Some(Orientation::Portrait), t.portrait);
            ui.selectable_value(&mut template.orientation, Some(Orientation::Landscape), t.landscape);
        });

        ui.add_space(8.0);
        ui.separator();
        let Some(fi) = self.field.filter(|&i| i < template.fields.len()) else {
            ui.weak(t.pick_field);
            return;
        };
        let f = &mut template.fields[fi];
        ui.label(RichText::new(t.field).strong());
        ui.label(t.name);
        name_input(ui, &mut f.name, problems.fields.get(&(entry.id, fi)).copied(), t);
        ui.label(t.sample).on_hover_text(t.sample_hint);
        let hint = f.name.clone();
        ui.add(TextEdit::singleline(&mut f.sample).hint_text(hint).desired_width(f32::INFINITY));

        ui.add_space(6.0);
        ui.label(t.zone);
        Grid::new("zone").num_columns(4).spacing([6.0, 4.0]).show(ui, |ui| {
            let (max_x, max_y) = (1.0 - f.w, 1.0 - f.h);
            ui.label("x");
            ui.add(fraction(&mut f.x, max_x));
            ui.label("y");
            ui.add(fraction(&mut f.y, max_y));
            ui.end_row();
            let (max_w, max_h) = (1.0 - f.x, 1.0 - f.y);
            ui.label("w");
            ui.add(fraction(&mut f.w, max_w).range(MIN_SIDE..=max_w));
            ui.label("h");
            ui.add(fraction(&mut f.h, max_h).range(MIN_SIDE..=max_h));
            ui.end_row();
        });

        ui.add_space(6.0);
        ui.label(t.align);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut f.align, FieldAlign::Left, t.align_left);
            ui.selectable_value(&mut f.align, FieldAlign::Center, t.align_center);
            ui.selectable_value(&mut f.align, FieldAlign::Right, t.align_right);
        });
        ui.label(t.fit);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut f.fit, FieldFit::Line, t.fit_line).on_hover_text(t.fit_line_hint);
            ui.selectable_value(&mut f.fit, FieldFit::Ink, t.fit_ink).on_hover_text(t.fit_ink_hint);
        });
        ui.add_space(4.0);
        ui.checkbox(&mut f.bold, t.bold);
        ui.checkbox(&mut f.wrap, t.wrap).on_hover_text(t.wrap_hint);
        ui.horizontal(|ui| {
            let mut limited = f.max_pt.is_some();
            if ui.checkbox(&mut limited, t.limit_size).changed() {
                f.max_pt = limited.then_some(24.0);
            }
            if let Some(pt) = &mut f.max_pt {
                ui.add(DragValue::new(pt).range(1.0..=300.0).speed(0.5).max_decimals(1).suffix(" pt"));
            }
        });
    }

    fn rerender(&mut self, ctx: &egui::Context, fonts: &Fonts, key: RenderKey) {
        if self.rendered.as_ref().is_some_and(|(k, ..)| *k == key) {
            return;
        }
        let r = key.render(fonts);
        let texture = app::texture(ctx, "template-editor-preview", &r.preview);
        self.rendered = Some((key, r, texture));
    }

    fn canvas(&mut self, ui: &mut Ui, t: &Tr, fonts: &Fonts, sizes: &[Size]) {
        ui.horizontal_wrapped(|ui| {
            ui.label(t.size);
            ComboBox::from_id_salt("editor-size").selected_text(self.size.as_str()).show_ui(ui, |ui| {
                for s in sizes {
                    let title = s.title();
                    ui.selectable_value(&mut self.size, title.clone(), title);
                }
            });
            ui.separator();
            ui.selectable_value(&mut self.orientation, Orientation::Portrait, t.portrait);
            ui.selectable_value(&mut self.orientation, Orientation::Landscape, t.landscape);
        });
        ui.horizontal_wrapped(|ui| {
            ui.weak(t.canvas_hint);
            ui.label(RichText::new(t.keys).weak().underline()).on_hover_text(t.keys_help);
        });

        let Some(ti) = self.current() else { return };
        let Some(size) = sizes.iter().find(|s| s.title() == self.size).or(sizes.first()).cloned() else { return };
        let template = self.draft[ti].template.clone();
        let texts = template.fields.iter().map(|f| if f.sample.is_empty() { f.name.clone() } else { f.sample.clone() }).collect();
        self.rerender(ui.ctx(), fonts, RenderKey { size, template, orientation: self.orientation, texts });
        let Some((_, r, texture)) = &self.rendered else { return };
        let px = Vec2::new(r.preview.width_px as f32, r.preview.height_px as f32);
        let texture = texture.clone();

        // Only what is visible: a toolbar wider than the panel must not push the canvas under the side panels.
        let area = ui.available_rect_before_wrap().intersect(ui.clip_rect());
        let background = ui.allocate_rect(area, Sense::click_and_drag());
        let label = app::paint_label(ui, area, &texture, px);
        let margin = (MARGIN_MM * DPI as f32 / 25.4).round() * label.width() / px.x;
        self.zones(ui, label.shrink(margin), &background, t);
    }

    /// Frames of the fields over the preview, and dragging them around.
    fn zones(&mut self, ui: &mut Ui, content: Rect, background: &Response, t: &Tr) {
        let Some(ti) = self.current() else { return };
        let id = self.draft[ti].id;
        let to_frac = |p: Pos2| Pos2::new((p.x - content.min.x) / content.width(), (p.y - content.min.y) / content.height());
        let to_screen = |e: Edges| {
            Rect::from_min_max(content.lerp_inside(Vec2::new(e.l, e.t)), content.lerp_inside(Vec2::new(e.r, e.b)))
        };
        let n = self.draft[ti].template.fields.len();
        let edges: Vec<Edges> = self.draft[ti].template.fields.iter().map(Edges::of).collect();
        let origin = ui.input(|i| i.pointer.press_origin()).map(to_frac).unwrap_or_default();

        if background.drag_started() {
            self.drag = Some(Drag { kind: DragKind::Create, start: Edges { l: 0.0, t: 0.0, r: 0.0, b: 0.0 }, origin });
        }
        if background.clicked() {
            self.field = None;
        }
        // The selected zone goes last so that it wins where zones overlap.
        let selected = self.field.filter(|&i| i < n);
        let order: Vec<usize> = (0..n).filter(|&i| Some(i) != selected).chain(selected).collect();
        for i in order {
            let response = ui.interact(to_screen(edges[i]), Id::new(("zone", id, i)), Sense::click_and_drag());
            if response.clicked() || response.drag_started() {
                self.field = Some(i);
            }
            if response.drag_started() {
                self.drag = Some(Drag { kind: DragKind::Zone { field: i, grab: Grab::Move }, start: edges[i], origin });
            }
            response.on_hover_cursor(CursorIcon::Move);
        }
        if let Some(i) = self.field.filter(|&i| i < n) {
            for (k, (grab, pos, cursor)) in grips(to_screen(edges[i])).into_iter().enumerate() {
                let rect = Rect::from_center_size(pos, Vec2::splat(12.0));
                let response = ui.interact(rect, Id::new(("grip", id, i, k)), Sense::drag());
                if response.drag_started() {
                    self.drag = Some(Drag { kind: DragKind::Zone { field: i, grab }, start: edges[i], origin });
                }
                response.on_hover_cursor(cursor);
            }
        }

        let (pointer, down, alt) =
            ui.input(|i| (i.pointer.latest_pos().map(to_frac), i.pointer.primary_down(), i.modifiers.alt));
        let mut band = None;
        if let Some(drag) = &self.drag
            && let Some(p) = pointer
        {
            let tol = Vec2::new(SNAP_PX / content.width(), SNAP_PX / content.height());
            let fields = &mut self.draft[ti].template.fields;
            match drag.kind {
                DragKind::Zone { field, grab } if field < fields.len() => {
                    let others = edges.iter().enumerate().filter(|&(j, _)| j != field).map(|(_, e)| *e);
                    let snap = (!alt).then(|| Snap::new(others, tol));
                    dragged(drag.start, grab, p - drag.origin, snap.as_ref()).apply(&mut fields[field]);
                }
                DragKind::Zone { .. } => {}
                DragKind::Create => {
                    let snap = (!alt).then(|| Snap::new(edges.iter().copied(), tol));
                    let e = spanned(drag.origin, p, snap.as_ref());
                    if !down && e.r - e.l >= MIN_SIDE && e.b - e.t >= MIN_SIDE {
                        let taken: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
                        let mut f = Field { name: unique_name(t.new_field, &taken), ..Field::default() };
                        e.apply(&mut f);
                        fields.push(f);
                        self.field = Some(fields.len() - 1);
                    }
                    band = Some(e);
                }
            }
        }
        if !down {
            self.drag = None;
            band = None;
        }

        let painter = ui.painter_at(content.expand(8.0));
        let faint = Color32::from_rgba_unmultiplied(0x2f, 0x81, 0xf7, 120);
        for (i, f) in self.draft[ti].template.fields.iter().enumerate() {
            let rect = to_screen(Edges::of(f));
            let selected = self.field == Some(i);
            let stroke = if selected { Stroke::new(2.0, ACCENT) } else { Stroke::new(1.0, faint) };
            painter.rect_stroke(rect, 0.0, stroke, StrokeKind::Middle);
            let color = if selected { ACCENT } else { faint };
            painter.text(rect.left_top() + Vec2::new(3.0, 2.0), Align2::LEFT_TOP, &f.name, FontId::proportional(11.0), color);
            if selected {
                for (_, pos, _) in grips(rect) {
                    let grip = Rect::from_center_size(pos, Vec2::splat(7.0));
                    painter.rect_filled(grip, 1.0, Color32::WHITE);
                    painter.rect_stroke(grip, 1.0, Stroke::new(1.5, ACCENT), StrokeKind::Middle);
                }
            }
        }
        if let Some(e) = band {
            let rect = to_screen(e);
            painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(0x2f, 0x81, 0xf7, 30));
            painter.rect_stroke(rect, 0.0, Stroke::new(1.0, ACCENT), StrokeKind::Middle);
        }
        if self.drag.is_some() {
            ui.ctx().request_repaint();
        }
    }

    fn closing_question(&mut self, ctx: &egui::Context, t: &Tr, saved: &mut Option<Vec<Template>>) {
        let Some(quitting) = self.closing else { return };
        #[derive(PartialEq)]
        enum Choice {
            Save,
            Discard,
            Cancel,
        }
        let mut choice = None;
        let can_save = self.can_save();
        egui::Modal::new(Id::new("unsaved-templates")).show(ctx, |ui| {
            ui.label(RichText::new(t.unsaved).strong().size(15.0));
            if !can_save {
                ui.colored_label(ui.visuals().error_fg_color, if self.writer.is_none() { t.no_file } else { t.fix_errors });
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(can_save, Button::new(t.save)).clicked() {
                    choice = Some(Choice::Save);
                }
                if ui.button(t.dont_save).clicked() {
                    choice = Some(Choice::Discard);
                }
                if ui.button(t.cancel).clicked() {
                    choice = Some(Choice::Cancel);
                }
            });
        });
        let close = match choice {
            Some(Choice::Save) => {
                *saved = self.save();
                saved.is_some()
            }
            Some(Choice::Discard) => {
                self.draft = self.baseline.clone();
                true
            }
            Some(Choice::Cancel) => {
                self.closing = None;
                false
            }
            None => false,
        };
        if close {
            self.closing = None;
            self.open = false;
            if quitting {
                ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Close);
            }
        }
    }
}

fn fraction(v: &mut f32, max: f32) -> DragValue<'_> {
    DragValue::new(v).range(0.0..=max).speed(0.002).max_decimals(3)
}

/// A name for a list: greyed out when empty, red when it cannot be saved.
fn list_item(ui: &Ui, name: &str, t: &Tr, bad: bool) -> RichText {
    let text = if name.trim().is_empty() { RichText::new(t.unnamed).italics() } else { RichText::new(name) };
    if bad { text.color(ui.visuals().error_fg_color) } else { text }
}

fn name_input(ui: &mut Ui, name: &mut String, problem: Option<Problem>, t: &Tr) {
    let mut edit = TextEdit::singleline(name).desired_width(f32::INFINITY);
    if problem.is_some() {
        edit = edit.background_color(ui.visuals().error_fg_color.gamma_multiply(0.15));
    }
    ui.add(edit);
    if let Some(problem) = problem {
        let text = match problem {
            Problem::Empty => t.empty_name,
            Problem::Taken => t.taken_name,
        };
        ui.colored_label(ui.visuals().error_fg_color, text);
    }
}

/// Corner and edge-middle grips of a zone: what each one moves, where it is, its cursor.
fn grips(r: Rect) -> [(Grab, Pos2, CursorIcon); 8] {
    let grab = |l, t, r, b| Grab::Resize { l, t, r, b };
    [
        (grab(true, true, false, false), r.left_top(), CursorIcon::ResizeNwSe),
        (grab(false, true, true, false), r.right_top(), CursorIcon::ResizeNeSw),
        (grab(true, false, false, true), r.left_bottom(), CursorIcon::ResizeNeSw),
        (grab(false, false, true, true), r.right_bottom(), CursorIcon::ResizeNwSe),
        (grab(false, true, false, false), r.center_top(), CursorIcon::ResizeVertical),
        (grab(false, false, false, true), r.center_bottom(), CursorIcon::ResizeVertical),
        (grab(true, false, false, false), r.left_center(), CursorIcon::ResizeHorizontal),
        (grab(false, false, true, false), r.right_center(), CursorIcon::ResizeHorizontal),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    fn edges(l: f32, t: f32, r: f32, b: f32) -> Edges {
        Edges { l, t, r, b }
    }

    fn close(a: Edges, b: Edges) -> bool {
        [(a.l, b.l), (a.t, b.t), (a.r, b.r), (a.b, b.b)].iter().all(|(x, y)| (x - y).abs() < 1e-5)
    }

    fn snap(others: &[Edges]) -> Snap {
        Snap::new(others.iter().copied(), Vec2::splat(0.02))
    }

    #[test]
    fn move_sticks_to_a_neighbour_edge() {
        let start = edges(0.0, 0.0, 0.5, 0.2);
        let neighbour = edges(0.0, 0.5, 1.0, 0.7);
        // Bottom edge comes within the tolerance of the neighbour's top.
        let moved = dragged(start, Grab::Move, Vec2::new(0.0, 0.287), Some(&snap(&[neighbour])));
        assert!(close(moved, edges(0.0, 0.3, 0.5, 0.5)), "{moved:?}");
    }

    #[test]
    fn move_falls_back_to_the_grid() {
        let moved = dragged(edges(0.0, 0.0, 0.5, 0.2), Grab::Move, Vec2::new(0.123, 0.456), Some(&snap(&[])));
        assert!(close(moved, edges(0.12, 0.46, 0.62, 0.66)), "{moved:?}");
    }

    #[test]
    fn alt_drag_does_not_snap() {
        let moved = dragged(edges(0.0, 0.0, 0.5, 0.2), Grab::Move, Vec2::new(0.123, 0.456), None);
        assert!(close(moved, edges(0.123, 0.456, 0.623, 0.656)), "{moved:?}");
    }

    #[test]
    fn move_stays_inside_the_area() {
        let moved = dragged(edges(0.2, 0.2, 0.6, 0.4), Grab::Move, Vec2::new(0.9, -0.9), Some(&snap(&[])));
        assert!(close(moved, edges(0.6, 0.0, 1.0, 0.2)), "{moved:?}");
    }

    #[test]
    fn resize_moves_only_grabbed_edges_and_keeps_a_minimum() {
        let start = edges(0.2, 0.2, 0.6, 0.4);
        let corner = Grab::Resize { l: false, t: false, r: true, b: true };
        let grown = dragged(start, corner, Vec2::new(0.1, 0.1), Some(&snap(&[])));
        assert!(close(grown, edges(0.2, 0.2, 0.7, 0.5)), "{grown:?}");
        let crushed = dragged(start, corner, Vec2::new(-1.0, -1.0), Some(&snap(&[])));
        assert!(close(crushed, edges(0.2, 0.2, 0.2 + MIN_SIDE, 0.2 + MIN_SIDE)), "{crushed:?}");
        let left = Grab::Resize { l: true, t: false, r: false, b: false };
        let past_edge = dragged(start, left, Vec2::new(-0.5, 0.0), Some(&snap(&[])));
        assert!(close(past_edge, edges(0.0, 0.2, 0.6, 0.4)), "{past_edge:?}");
    }

    #[test]
    fn new_zone_spans_either_direction() {
        let e = spanned(Pos2::new(0.8, 0.9), Pos2::new(0.1, 0.2), Some(&snap(&[])));
        assert!(close(e, edges(0.1, 0.2, 0.8, 0.9)), "{e:?}");
    }

    #[test]
    fn free_band_finds_the_largest_gap() {
        let field = |y: f32, h: f32| Field { y, h, ..Field::default() };
        let band = free_band(&[field(0.0, 0.2), field(0.3, 0.1), field(0.8, 0.2)]);
        assert!(close(band, edges(0.0, 0.4, 1.0, 0.8)), "{band:?}");
        let full = free_band(&[field(0.0, 1.0)]);
        assert!(close(full, edges(0.0, 0.0, 1.0, 0.2)), "nowhere free: a band at the top, {full:?}");
        assert!(close(free_band(&[]), edges(0.0, 0.0, 1.0, 1.0)));
    }

    #[test]
    fn names_get_a_free_suffix() {
        assert_eq!(unique_name("Поле", &[]), "Поле");
        assert_eq!(unique_name("Поле", &["Поле", "Поле 2"]), "Поле 3");
    }

    #[test]
    fn empty_and_repeated_names_are_flagged() {
        let p = name_problems(["A", "", "B", "A", " "].into_iter());
        assert_eq!(p, [Some(Problem::Taken), Some(Problem::Empty), None, Some(Problem::Taken), Some(Problem::Empty)]);
    }

    fn template(name: &str) -> Template {
        Template { name: name.into(), orientation: None, fields: vec![Field { name: "F".into(), ..Field::default() }] }
    }

    #[test]
    fn undo_steps_back_and_forward() {
        let mut e = Editor::default();
        e.open(&[template("A")], None, "A", "", Orientation::Portrait);
        e.draft[0].template.name = "B".into();
        e.undo.push(mem::replace(&mut e.committed, e.draft.clone()));
        e.step_back();
        assert_eq!(e.draft[0].template.name, "A");
        assert!(!e.dirty());
        e.step_forward();
        assert_eq!(e.draft[0].template.name, "B");
        assert!(e.dirty());
    }

    /// Runs one editor frame with `key` pressed.
    fn press(e: &mut Editor, ctx: &egui::Context, modifiers: Modifiers, key: Key) {
        let event = egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers };
        let input = egui::RawInput { events: vec![event], modifiers, ..Default::default() };
        let (fonts, sizes) = (Fonts::embedded(), config::builtin_sizes());
        let _ = ctx.run_ui(input, |ui| {
            e.show(ui, &crate::i18n::EN, &fonts, &sizes);
        });
    }

    fn badge_editor(path: Option<&std::path::Path>) -> (Editor, egui::Context) {
        let ctx = egui::Context::default();
        crate::install_fonts(&ctx);
        let mut e = Editor::default();
        let writer = path.map(|p| TemplatesWriter(p.to_path_buf()));
        e.open(&config::builtin_templates(), writer, "Badge", "", Orientation::Landscape);
        (e, ctx)
    }

    fn names(e: &Editor) -> Vec<&str> {
        e.draft[e.current().unwrap()].template.fields.iter().map(|f| f.name.as_str()).collect()
    }

    #[test]
    fn delete_and_backspace_remove_the_selected_field() {
        let (mut e, ctx) = badge_editor(None);
        e.field = Some(1);
        press(&mut e, &ctx, Modifiers::NONE, Key::Delete);
        assert_eq!(names(&e), ["Team number", "Team", "University"]);
        assert_eq!(e.field, Some(1), "the next field gets selected");
        e.field = Some(2);
        press(&mut e, &ctx, Modifiers::NONE, Key::Backspace);
        assert_eq!(names(&e), ["Team number", "Team"]);
        assert_eq!(e.field, Some(1), "after the last one, the new last");
        e.field = None;
        press(&mut e, &ctx, Modifiers::NONE, Key::Delete);
        assert_eq!(names(&e).len(), 2, "nothing selected, nothing deleted");
        press(&mut e, &ctx, Modifiers::COMMAND, Key::Z);
        assert_eq!(names(&e), ["Team number", "Team", "University"], "undo brings it back");
    }

    #[test]
    fn keys_go_to_the_input_box_while_typing() {
        let (mut e, ctx) = badge_editor(None);
        e.field = Some(1);
        e.typing = true;
        press(&mut e, &ctx, Modifiers::NONE, Key::Backspace);
        assert_eq!(names(&e).len(), 4);
    }

    #[test]
    fn arrows_move_and_alt_arrows_resize() {
        let (mut e, ctx) = badge_editor(None);
        e.field = Some(2); // Team: x 0, y 0.7, w 1, h 0.14
        let zone = |e: &Editor| Edges::of(&e.draft[e.current().unwrap()].template.fields[2]);
        press(&mut e, &ctx, Modifiers::NONE, Key::ArrowUp);
        assert!(close(zone(&e), edges(0.0, 0.69, 1.0, 0.83)), "{:?}", zone(&e));
        press(&mut e, &ctx, Modifiers::SHIFT, Key::ArrowDown);
        assert!(close(zone(&e), edges(0.0, 0.79, 1.0, 0.93)), "{:?}", zone(&e));
        press(&mut e, &ctx, Modifiers::NONE, Key::ArrowRight);
        assert!(close(zone(&e), edges(0.0, 0.79, 1.0, 0.93)), "already at the right edge: {:?}", zone(&e));
        press(&mut e, &ctx, Modifiers::ALT, Key::ArrowLeft);
        assert!(close(zone(&e), edges(0.0, 0.79, 0.99, 0.93)), "{:?}", zone(&e));
        press(&mut e, &ctx, Modifiers::ALT | Modifiers::SHIFT, Key::ArrowUp);
        assert!(close(zone(&e), edges(0.0, 0.79, 0.99, 0.83)), "{:?}", zone(&e));
    }

    #[test]
    fn tab_walks_fields_and_esc_deselects() {
        let (mut e, ctx) = badge_editor(None);
        press(&mut e, &ctx, Modifiers::NONE, Key::Tab);
        assert_eq!(e.field, Some(0));
        press(&mut e, &ctx, Modifiers::SHIFT, Key::Tab);
        assert_eq!(e.field, Some(3), "wraps around backwards");
        press(&mut e, &ctx, Modifiers::NONE, Key::Tab);
        assert_eq!(e.field, Some(0), "and forwards");
        press(&mut e, &ctx, Modifiers::NONE, Key::Escape);
        assert_eq!(e.field, None);
    }

    #[test]
    fn cmd_d_duplicates_next_to_the_original() {
        let (mut e, ctx) = badge_editor(None);
        e.field = Some(3);
        press(&mut e, &ctx, Modifiers::COMMAND, Key::D);
        assert_eq!(names(&e), ["Team number", "Name", "Team", "University", "University 2"]);
        assert_eq!(e.field, Some(4));
        let fields = &e.draft[e.current().unwrap()].template.fields;
        assert!(close(Edges::of(&fields[4]), edges(0.0, 0.86, 1.0, 1.0)), "pushed back inside: {:?}", Edges::of(&fields[4]));
    }

    #[test]
    fn cmd_s_saves_only_a_valid_draft() {
        let path = std::env::temp_dir().join(format!("itmo-labels-editor-test-{}.toml", std::process::id()));
        let (mut e, ctx) = badge_editor(Some(&path));
        e.draft[0].template.name = e.draft[1].template.name.clone();
        press(&mut e, &ctx, Modifiers::COMMAND, Key::S);
        assert!(!path.exists(), "duplicate names are not written");
        e.draft[0].template.name = "Renamed".into();
        press(&mut e, &ctx, Modifiers::COMMAND, Key::S);
        let written = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(written.contains("name = \"Renamed\""));
        assert!(!e.dirty());
    }

    #[test]
    fn unsaved_draft_wins_over_the_file() {
        let mut e = Editor::default();
        e.open(&[template("A")], None, "A", "", Orientation::Portrait);
        e.file_changed(&[template("From disk")]);
        assert_eq!(e.draft[0].template.name, "From disk", "nothing unsaved: follow the file");
        e.draft[0].template.name = "Mine".into();
        e.file_changed(&[template("Again from disk")]);
        assert_eq!(e.draft[0].template.name, "Mine");
    }
}
