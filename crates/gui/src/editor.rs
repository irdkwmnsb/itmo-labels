//! Template editor: a window of its own that edits a draft of the whole `templates.toml`.
//! The main window renders the draft as it changes; the file is only written on Save.

use std::{
    collections::{HashMap, HashSet},
    mem,
};

use eframe::egui::{
    self, Align2, Button, CentralPanel, Color32, ComboBox, CursorIcon, DragValue, FontId, Grid, Id, Key,
    Modifiers, Panel, Pos2, Rect, Response, RichText, ScrollArea, Sense, Stroke, StrokeKind,
    TextEdit, TextureHandle, Ui, Vec2, ViewportCommand, ViewportId,
};
use itmo_labels_core::render::{MARGIN_MM, Rendered};

use crate::{
    app::{self, DPI, Fonts, RenderKey, pending_fresh, pending_update},
    config::{Anchor, Field, FieldAlign, FieldFit, Orientation, Picture, Size, Template, TemplatesWriter},
    i18n::Tr,
    picture::{FilePicker, Mode, Pending, Settings, Source},
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
/// Frames of pictures that are not baked yet.
const PENDING: Color32 = Color32::from_rgb(0xd2, 0x99, 0x22);
/// Narrowest picture, in dots (1 mm).
const MIN_PICTURE_PX: u32 = 8;

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

fn px_to_mm(px: u32) -> f32 {
    px as f32 * 25.4 / DPI as f32
}

fn mm_to_px(mm: f32) -> u32 {
    (mm * DPI as f32 / 25.4).round().max(MIN_PICTURE_PX as f32) as u32
}

/// A picture's box on a label of `label` dots, in fractions of the label.
fn picture_box(p: &Picture, label: Vec2) -> Edges {
    let (x, y) = p.core().origin(label.x as u32, label.y as u32);
    let (x, y) = (x as f32, y as f32);
    let (w, h) = p.intended_size();
    let (w, h) = (w as f32, h as f32);
    Edges { l: x / label.x, t: y / label.y, r: (x + w) / label.x, b: (y + h) / label.y }
}

/// Puts the picture's top-left corner at (l, t), in fractions of the label, picking the
/// anchor anew unless it is locked. The picture may hang off the label (it is cut off
/// at print), but a sliver must stay on it, so it can be grabbed and pulled back.
fn place(p: &mut Picture, l: f32, t: f32, label: Vec2) {
    if label.x < 1.0 || label.y < 1.0 {
        return;
    }
    let (w, h) = p.intended_size();
    let (w, h) = (w as f32 / label.x, h as f32 / label.y);
    if !p.anchor_locked {
        p.anchor = Anchor::nearest(l + w / 2.0, t + h / 2.0);
    }
    let (ax, ay) = p.anchor.fractions();
    let l = clamp_visible(l, w);
    let t = clamp_visible(t, h);
    p.x = l + ax * w;
    p.y = t + ay * h;
}

/// How much of a picture hanging off the label must stay reachable: this fraction of the
/// label, and never more than the picture itself (small ones stay fully on the label).
const MIN_VISIBLE: f32 = 0.02;

/// Keeps at least a sliver of a `w`-wide box (fractions of the label) on the label.
fn clamp_visible(l: f32, w: f32) -> f32 {
    let sliver = w.min(MIN_VISIBLE);
    l.clamp(sliver - w, 1.0 - sliver)
}

/// Resizes a picture being placed by its corner, keeping the opposite corner of `start` still.
/// The bitmap itself catches up on the next refresh tick (conversion is rate-limited while
/// dragging); the box follows the pointer by the intended size, so there is no visible lag.
fn resize_picture(p: &mut Picture, start: Edges, right: bool, bottom: bool, pointer: Pos2, label: Vec2) {
    let fixed = Pos2::new(if right { start.l } else { start.r }, if bottom { start.t } else { start.b });
    {
        let Some(pending) = &mut p.pending else { return };
        let (sw, sh) = pending.source.size();
        let by_x = (pointer.x - fixed.x).abs() * label.x;
        let by_y = (pointer.y - fixed.y).abs() * label.y * sw as f32 / sh as f32;
        pending.settings.width_px = (by_x.max(by_y).round() as u32).max(MIN_PICTURE_PX);
    }
    let (w, h) = p.intended_size();
    let (w, h) = (w as f32 / label.x, h as f32 / label.y);
    let l = if right { fixed.x } else { fixed.x - w };
    let t = if bottom { fixed.y } else { fixed.y - h };
    place(p, l, t, label);
}

/// Turns the picture a quarter clockwise around its anchor point.
fn rotate_picture(p: &mut Picture) {
    match &mut p.pending {
        Some(pending) => {
            pending.rotate();
            p.refresh_now();
        }
        None => p.bitmap = p.bitmap.rotated_cw(),
    }
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
    /// Templates with pictures not baked yet: they cannot be saved, but the main window shows them.
    pending: HashSet<u64>,
}

impl Problems {
    fn of(draft: &[Entry]) -> Self {
        let mut p = Self::default();
        for (e, problem) in draft.iter().zip(name_problems(draft.iter().map(|e| e.template.name.as_str()))) {
            if let Some(problem) = problem {
                p.templates.insert(e.id, problem);
            }
            if e.template.pictures.iter().any(|p| p.pending.is_some()) {
                p.pending.insert(e.id);
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

    /// No naming errors; unbaked pictures do not count.
    fn is_empty(&self) -> bool {
        self.templates.is_empty() && self.fields.is_empty()
    }

    fn blocks_save(&self) -> bool {
        !self.is_empty() || !self.pending.is_empty()
    }

    /// Why the draft cannot be saved, if it cannot.
    fn blocker<'t>(&self, t: &'t Tr) -> Option<&'t str> {
        if !self.is_empty() {
            Some(t.fix_errors)
        } else if !self.pending.is_empty() {
            Some(t.bake_first)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum PictureGrab {
    Move,
    /// Which corner follows the pointer; only while the picture is being placed.
    Corner { right: bool, bottom: bool },
}

enum DragKind {
    Zone { field: usize, grab: Grab },
    Create,
    Picture { picture: usize, grab: PictureGrab },
}

struct Drag {
    kind: DragKind,
    /// In fractions of the area for zones, of the whole label for pictures.
    start: Edges,
    /// Where the pointer went down, same units as `start`.
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
    /// At most one of `field` and `picture` is selected.
    field: Option<usize>,
    picture: Option<usize>,
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
    /// The preview label in dots, as read: what picture fractions are of.
    label_px: Vec2,
    picker: FilePicker,
    import_error: Option<String>,
    /// Live bitmaps of pictures still being placed: (template, index) → texture.
    /// Kept out of the label render, so scaling one does not re-render the whole label.
    picture_textures: HashMap<(String, usize), app::PendingTexture>,
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
            self.picture = None;
        }
        let (fields, pictures) =
            self.current().map_or((0, 0), |i| (self.draft[i].template.fields.len(), self.draft[i].template.pictures.len()));
        self.field = self.field.filter(|&f| f < fields);
        self.picture = self.picture.filter(|&p| p < pictures && self.field.is_none());
    }

    fn can_save(&self) -> bool {
        self.writer.is_some() && !Problems::of(&self.draft).blocks_save()
    }

    /// After a save the sources are gone: earlier undo steps keep pictures as they looked then, baked.
    fn forget_sources(&mut self) {
        let steps = self.undo.iter_mut().chain(self.redo.iter_mut()).chain([&mut self.committed]);
        for e in steps.flatten() {
            for p in &mut e.template.pictures {
                p.pending = None;
            }
        }
    }

    fn save(&mut self) -> Option<Vec<Template>> {
        let writer = self.writer.as_ref()?;
        let templates: Vec<Template> = self.draft.iter().map(|e| e.template.clone()).collect();
        match writer.write(&templates) {
            Ok(()) => {
                self.before_save = Some(mem::replace(&mut self.baseline, self.draft.clone()));
                self.save_error = None;
                self.forget_sources();
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
        let template = &mut self.draft[ti].template;

        // Tab walks the fields, then the pictures.
        let prev = key(Modifiers::SHIFT, Key::Tab);
        let next = !prev && key(Modifiers::NONE, Key::Tab);
        let (nf, n) = (template.fields.len(), template.fields.len() + template.pictures.len());
        if (next || prev) && n > 0 {
            let at = self.field.or(self.picture.map(|p| nf + p));
            let to = match (at, prev) {
                (Some(i), false) => (i + 1) % n,
                (Some(i), true) => (i + n - 1) % n,
                (None, false) => 0,
                (None, true) => n - 1,
            };
            (self.field, self.picture) = if to < nf { (Some(to), None) } else { (None, Some(to - nf)) };
        }
        if key(Modifiers::NONE, Key::Escape) {
            self.field = None;
            self.picture = None;
        }
        if let Some(pi) = self.picture.filter(|&i| i < template.pictures.len()) {
            let (pictures, label) = (&mut template.pictures, self.label_px);
            if key(Modifiers::NONE, Key::Delete) || key(Modifiers::NONE, Key::Backspace) {
                pictures.remove(pi);
                self.picture = (!pictures.is_empty()).then(|| pi.min(pictures.len() - 1));
            } else if key(Modifiers::COMMAND, Key::D) {
                let mut copy = pictures[pi].clone();
                let b = picture_box(&copy, label);
                place(&mut copy, b.l + 2.0 * NUDGE, b.t + 2.0 * NUDGE, label);
                pictures.insert(pi + 1, copy);
                self.picture = Some(pi + 1);
            } else {
                let arrows = [(Key::ArrowLeft, -1.0, 0.0), (Key::ArrowRight, 1.0, 0.0), (Key::ArrowUp, 0.0, -1.0), (Key::ArrowDown, 0.0, 1.0)];
                for (arrow, dx, dy) in arrows {
                    let step = if key(Modifiers::SHIFT, arrow) {
                        NUDGE_FAR
                    } else if key(Modifiers::NONE, arrow) {
                        NUDGE
                    } else {
                        continue;
                    };
                    let p = &mut pictures[pi];
                    let b = picture_box(p, label);
                    place(p, b.l + dx * step, b.t + dy * step, label);
                }
            }
            Self::keep_focus_still(ctx);
            return saved;
        }
        let fields = &mut template.fields;
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
        match self.picker.take() {
            Some(Ok(bytes)) => self.add_picture(&bytes, None),
            Some(Err(e)) => self.import_error = Some(e),
            None => {}
        }
        // Pending pictures convert here without blocking: a worker catches the bitmap
        // up with the settings after a drag settles.
        for e in &mut self.draft {
            for p in &mut e.template.pictures {
                p.refresh();
            }
        }

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
            } else if let Some(why) = problems.blocker(t) {
                ui.colored_label(ui.visuals().error_fg_color, why);
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
                    pictures: Vec::new(),
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
            let bad = problems.templates.contains_key(&e.id) || problems.pending.contains(&e.id);
            let text = list_item(ui, &e.template.name, t, bad);
            if ui.selectable_label(Some(e.id) == self.selected, text).clicked() && Some(e.id) != self.selected {
                self.selected = Some(e.id);
                self.field = None;
                self.picture = None;
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
                self.picture = None;
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
                self.picture = None;
            }
        }

        ui.add_space(8.0);
        ui.separator();
        ui.label(RichText::new(t.pictures_heading).strong());
        let picture = self.picture;
        ui.horizontal_wrapped(|ui| {
            if ui.small_button(t.add).clicked() {
                self.picker.open(ui.ctx());
            }
            if ui.add_enabled(picture.is_some(), Button::new(t.delete).small()).clicked()
                && let Some(i) = picture
            {
                self.draft[ti].template.pictures.remove(i);
                self.picture = None;
            }
        });
        if let Some(e) = &self.import_error {
            ui.colored_label(ui.visuals().error_fg_color, format!("{} {e}", t.picture_failed));
        }
        for (i, p) in self.draft[ti].template.pictures.iter().enumerate() {
            let (w, h) = (px_to_mm(p.bitmap.width_px), px_to_mm(p.bitmap.height_px));
            let mut text = format!("{} {} · {w:.1}×{h:.1} {}", t.picture, i + 1, t.mm);
            if p.pending.is_some() {
                text = format!("{text} · {}", t.not_baked);
            }
            let text = if p.pending.is_some() { RichText::new(text).color(PENDING) } else { RichText::new(text) };
            if ui.selectable_label(self.picture == Some(i), text).clicked() {
                self.picture = Some(i);
                self.field = None;
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
        if let Some(pi) = self.picture.filter(|&i| i < template.pictures.len()) {
            picture_properties(ui, t, &mut template.pictures[pi], self.label_px);
            return;
        }
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

    /// Adds a picture from a PNG or JPEG file, centred at `at` (fractions of the label) or in the middle.
    fn add_picture(&mut self, bytes: &[u8], at: Option<Pos2>) {
        let Some(ti) = self.current() else { return };
        let source = match Source::decode(bytes) {
            Ok(source) => source,
            Err(e) => {
                self.import_error = Some(e);
                return;
            }
        };
        self.import_error = None;
        let label = self.label_px;
        // Fitted into half of the area inside the margins.
        let margins = 2.0 * (MARGIN_MM * DPI as f32 / 25.4).round();
        let fit = ((label - Vec2::splat(margins)) * 0.5).max(Vec2::splat(MIN_PICTURE_PX as f32));
        let (sw, sh) = source.size();
        let scale = (fit.x / sw as f32).min(fit.y / sh as f32);
        let width_px = ((sw as f32 * scale).round() as u32).max(MIN_PICTURE_PX);
        let settings = Settings { width_px, mode: Mode::Threshold, threshold: 128, invert: false };
        let (pending, bitmap) = Pending::new(source, settings);
        let mut p = Picture { x: 0.5, y: 0.5, anchor: Anchor::Center, anchor_locked: false, bitmap, pending: Some(pending) };
        let c = at.unwrap_or(Pos2::new(0.5, 0.5));
        let (w, h) = (p.bitmap.width_px as f32 / label.x, p.bitmap.height_px as f32 / label.y);
        place(&mut p, c.x - w / 2.0, c.y - h / 2.0, label);
        let pictures = &mut self.draft[ti].template.pictures;
        pictures.push(p);
        self.picture = Some(pictures.len() - 1);
        self.field = None;
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
        // Pictures still being place are drawn over the preview live, so they must not
        // be part of the render key: scaling one would re-render the whole label each frame.
        let mut template = self.draft[ti].template.clone();
        template.pictures.retain(|p| p.pending.is_none());
        let texts = template.fields.iter().map(|f| if f.sample.is_empty() { f.name.clone() } else { f.sample.clone() }).collect();
        self.rerender(ui.ctx(), fonts, RenderKey { size, template, orientation: self.orientation, texts });
        let Some((_, r, texture)) = &self.rendered else { return };
        let px = Vec2::new(r.preview.width_px as f32, r.preview.height_px as f32);
        let texture = texture.clone();

        // Only what is visible: a toolbar wider than the panel must not push the canvas under the side panels.
        let area = ui.available_rect_before_wrap().intersect(ui.clip_rect());
        let background = ui.allocate_rect(area, Sense::click_and_drag());
        let label = app::paint_label(ui, area, &texture, px);
        self.label_px = px;
        let margin = (MARGIN_MM * DPI as f32 / 25.4).round() * label.width() / px.x;
        self.zones(ui, label, label.shrink(margin), &background, t);
        self.dropped_files(ui, label, t);
    }

    /// Frames of the fields and pictures over the preview, and dragging them around.
    fn zones(&mut self, ui: &mut Ui, label: Rect, content: Rect, background: &Response, t: &Tr) {
        let Some(ti) = self.current() else { return };
        let id = self.draft[ti].id;
        let px = self.label_px;
        let to_frac = |p: Pos2| Pos2::new((p.x - content.min.x) / content.width(), (p.y - content.min.y) / content.height());
        let to_label = |p: Pos2| Pos2::new((p.x - label.min.x) / label.width(), (p.y - label.min.y) / label.height());
        let to_screen = |e: Edges| {
            Rect::from_min_max(content.lerp_inside(Vec2::new(e.l, e.t)), content.lerp_inside(Vec2::new(e.r, e.b)))
        };
        let label_to_screen = |e: Edges| {
            Rect::from_min_max(label.lerp_inside(Vec2::new(e.l, e.t)), label.lerp_inside(Vec2::new(e.r, e.b)))
        };
        // Zones are fractions of the area inside the margins, pictures of the whole label.
        let (m, k) = ((content.min - label.min) / label.size(), content.size() / label.size());
        let zone_to_label = |e: Edges| Edges { l: m.x + e.l * k.x, t: m.y + e.t * k.y, r: m.x + e.r * k.x, b: m.y + e.b * k.y };
        let label_to_zone = |e: Edges| Edges { l: (e.l - m.x) / k.x, t: (e.t - m.y) / k.y, r: (e.r - m.x) / k.x, b: (e.b - m.y) / k.y };

        let template = &self.draft[ti].template;
        let (n, np) = (template.fields.len(), template.pictures.len());
        let edges: Vec<Edges> = template.fields.iter().map(Edges::of).collect();
        let boxes: Vec<Edges> = template.pictures.iter().map(|p| picture_box(p, px)).collect();
        let press = ui.input(|i| i.pointer.press_origin());
        let origin = press.map(to_frac).unwrap_or_default();
        let origin_label = press.map(to_label).unwrap_or_default();

        if background.drag_started() {
            self.drag = Some(Drag { kind: DragKind::Create, start: Edges { l: 0.0, t: 0.0, r: 0.0, b: 0.0 }, origin });
        }
        if background.clicked() {
            self.field = None;
            self.picture = None;
        }
        // Pictures go over zones, and the selected item last, so that it wins where they overlap.
        let field = self.field.filter(|&i| i < n);
        let picture = self.picture.filter(|&i| i < np);
        let zone_order = (0..n).filter(|&i| Some(i) != field).map(|i| (false, i));
        let picture_order = (0..np).filter(|&i| Some(i) != picture).map(|i| (true, i));
        let selected = field.map(|i| (false, i)).or(picture.map(|i| (true, i)));
        let order: Vec<(bool, usize)> = zone_order.chain(picture_order).chain(selected).collect();
        for (is_picture, i) in order {
            let (rect, key) = if is_picture { (label_to_screen(boxes[i]), "picture") } else { (to_screen(edges[i]), "zone") };
            let response = ui.interact(rect, Id::new((key, id, i)), Sense::click_and_drag());
            if response.clicked() || response.drag_started() {
                (self.field, self.picture) = if is_picture { (None, Some(i)) } else { (Some(i), None) };
            }
            if response.drag_started() {
                self.drag = Some(if is_picture {
                    Drag { kind: DragKind::Picture { picture: i, grab: PictureGrab::Move }, start: boxes[i], origin: origin_label }
                } else {
                    Drag { kind: DragKind::Zone { field: i, grab: Grab::Move }, start: edges[i], origin }
                });
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
        if let Some(i) = self.picture.filter(|&i| i < np && self.draft[ti].template.pictures[i].pending.is_some()) {
            for (k, (grab, pos, cursor)) in corner_grips(label_to_screen(boxes[i])).into_iter().enumerate() {
                let rect = Rect::from_center_size(pos, Vec2::splat(12.0));
                let response = ui.interact(rect, Id::new(("corner", id, i, k)), Sense::drag());
                if response.drag_started() {
                    self.drag = Some(Drag { kind: DragKind::Picture { picture: i, grab }, start: boxes[i], origin: origin_label });
                }
                response.on_hover_cursor(cursor);
            }
        }

        let (screen_pointer, down, alt) = ui.input(|i| (i.pointer.latest_pos(), i.pointer.primary_down(), i.modifiers.alt));
        let mut band = None;
        if let Some(drag) = &self.drag
            && let Some(sp) = screen_pointer
        {
            let p = to_frac(sp);
            let tol = Vec2::new(SNAP_PX / content.width(), SNAP_PX / content.height());
            let template = &mut self.draft[ti].template;
            let fields = &mut template.fields;
            // Zones also stick to the pictures, and pictures to the zones and the margins.
            let picture_zones = || boxes.iter().map(|b| label_to_zone(*b));
            match drag.kind {
                DragKind::Zone { field, grab } if field < fields.len() => {
                    let others = edges.iter().enumerate().filter(|&(j, _)| j != field).map(|(_, e)| *e);
                    let snap = (!alt).then(|| Snap::new(others.chain(picture_zones()), tol));
                    dragged(drag.start, grab, p - drag.origin, snap.as_ref()).apply(&mut fields[field]);
                }
                DragKind::Create => {
                    let snap = (!alt).then(|| Snap::new(edges.iter().copied().chain(picture_zones()), tol));
                    let e = spanned(drag.origin, p, snap.as_ref());
                    if !down && e.r - e.l >= MIN_SIDE && e.b - e.t >= MIN_SIDE {
                        let taken: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
                        let mut f = Field { name: unique_name(t.new_field, &taken), ..Field::default() };
                        e.apply(&mut f);
                        fields.push(f);
                        self.field = Some(fields.len() - 1);
                        self.picture = None;
                    }
                    band = Some(e);
                }
                DragKind::Picture { picture, grab } if picture < template.pictures.len() => {
                    let p = to_label(sp);
                    let pic = &mut template.pictures[picture];
                    match grab {
                        PictureGrab::Move => {
                            let s = drag.start;
                            let (mut l, mut t) = (s.l + p.x - drag.origin.x, s.t + p.y - drag.origin.y);
                            if !alt {
                                let margins = Edges { l: m.x, t: m.y, r: m.x + k.x, b: m.y + k.y };
                                let zones = edges.iter().map(|e| zone_to_label(*e));
                                let others = boxes.iter().enumerate().filter(|&(j, _)| j != picture).map(|(_, b)| *b);
                                let snap = Snap::new(zones.chain(others).chain([margins]), Vec2::splat(SNAP_PX) / label.size());
                                l += snap_shift(l, l + s.r - s.l, &snap.xs, snap.tol.x);
                                t += snap_shift(t, t + s.b - s.t, &snap.ys, snap.tol.y);
                            }
                            place(pic, l, t, px);
                        }
                        PictureGrab::Corner { right, bottom } => resize_picture(pic, drag.start, right, bottom, p, px),
                    }
                }
                DragKind::Zone { .. } | DragKind::Picture { .. } => {}
            }
        }
        if !down {
            self.drag = None;
            band = None;
        }

        // A picture still being placed is drawn here, live, on top of the cached label render;
        // it may hang off the label, so both bitmap and frame paint over the whole canvas.
        let canvas_painter = ui.painter_at(background.rect);
        let mut live: HashSet<(String, usize)> = HashSet::new();
        for (i, p) in self.draft[ti].template.pictures.iter().enumerate() {
            let Some(pending) = &p.pending else { continue };
            let rect = label_to_screen(boxes[i]);
            let key = (self.draft[ti].template.name.clone(), i);
            let version = pending.version();
            if !pending_fresh(&self.picture_textures, &key, &version) {
                pending_update(ui.ctx(), &mut self.picture_textures, key.clone(), version, &p.bitmap);
            }
            let texture = self.picture_textures[&key].texture.id();
            let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
            canvas_painter.image(texture, rect, uv, Color32::WHITE);
            live.insert(key);
        }
        self.picture_textures.retain(|k, _| live.contains(k));

        let painter = ui.painter_at(label.expand(8.0));
        let picture_painter = ui.painter_at(background.rect);
        let faint = Color32::from_rgba_unmultiplied(0x2f, 0x81, 0xf7, 120);
        let template = &self.draft[ti].template;
        for (i, f) in template.fields.iter().enumerate() {
            let rect = to_screen(Edges::of(f));
            let selected = self.field == Some(i);
            let stroke = if selected { Stroke::new(2.0, ACCENT) } else { Stroke::new(1.0, faint) };
            painter.rect_stroke(rect, 0.0, stroke, StrokeKind::Middle);
            let color = if selected { ACCENT } else { faint };
            painter.text(rect.left_top() + Vec2::new(3.0, 2.0), Align2::LEFT_TOP, &f.name, FontId::proportional(11.0), color);
            if selected {
                for (_, pos, _) in grips(rect) {
                    paint_grip(&painter, pos);
                }
            }
        }
        for (i, p) in template.pictures.iter().enumerate() {
            let rect = label_to_screen(picture_box(p, px));
            let selected = self.picture == Some(i);
            let color = match (p.pending.is_some(), selected) {
                (true, _) => PENDING,
                (false, true) => ACCENT,
                (false, false) => faint,
            };
            picture_painter.rect_stroke(rect, 0.0, Stroke::new(if selected { 2.0 } else { 1.0 }, color), StrokeKind::Outside);
            if p.pending.is_some() {
                let text = format!("{} {} · {}", t.picture, i + 1, t.not_baked);
                picture_painter.text(rect.left_top() + Vec2::new(3.0, 2.0), Align2::LEFT_TOP, text, FontId::proportional(11.0), color);
                if selected {
                    for (_, pos, _) in corner_grips(rect) {
                        paint_grip(&picture_painter, pos);
                    }
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

    /// Files dropped onto the canvas become pictures centred where they were dropped.
    fn dropped_files(&mut self, ui: &Ui, label: Rect, t: &Tr) {
        let (hovering, dropped, pos) =
            ui.input(|i| (!i.raw.hovered_files.is_empty(), i.raw.dropped_files.clone(), i.pointer.latest_pos()));
        if hovering {
            ui.painter().rect_filled(label, 3.0, ACCENT.gamma_multiply(0.15));
            ui.painter().text(label.center(), Align2::CENTER_CENTER, t.drop_here, FontId::proportional(16.0), ACCENT);
        }
        let at = pos
            .filter(|p| label.contains(*p))
            .map(|p| Pos2::new((p.x - label.min.x) / label.width(), (p.y - label.min.y) / label.height()));
        for file in dropped {
            let bytes = match (&file.bytes, &file.path) {
                (Some(bytes), _) => Ok(bytes.to_vec()),
                #[cfg(not(target_arch = "wasm32"))]
                (None, Some(path)) => std::fs::read(path).map_err(|e| e.to_string()),
                _ => Err(file.name.clone()),
            };
            match bytes {
                Ok(bytes) => self.add_picture(&bytes, at),
                Err(e) => self.import_error = Some(e),
            }
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
        let blocker = Problems::of(&self.draft).blocker(t);
        egui::Modal::new(Id::new("unsaved-templates")).show(ctx, |ui| {
            ui.label(RichText::new(t.unsaved).strong().size(15.0));
            if !can_save {
                let why = if self.writer.is_none() { t.no_file } else { blocker.unwrap_or(t.fix_errors) };
                ui.colored_label(ui.visuals().error_fg_color, why);
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

fn picture_properties(ui: &mut Ui, t: &Tr, p: &mut Picture, label: Vec2) {
    ui.label(RichText::new(t.picture).strong());
    let (w, h) = (px_to_mm(p.bitmap.width_px), px_to_mm(p.bitmap.height_px));
    ui.label(format!("{}: {w:.1} × {h:.1} {}", t.picture_size, t.mm));
    if let Some(pending) = &mut p.pending {
        let s = &mut pending.settings;
        ui.horizontal(|ui| {
            ui.label(t.picture_width);
            let mut mm = px_to_mm(s.width_px);
            let value = DragValue::new(&mut mm).range(1.0..=1000.0).speed(0.1).max_decimals(1).suffix(format!(" {}", t.mm));
            if ui.add(value).changed() {
                s.width_px = mm_to_px(mm);
            }
        });
        ui.add_space(6.0);
        ui.label(t.mode);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut s.mode, Mode::Threshold, t.mode_threshold).on_hover_text(t.mode_threshold_hint);
            ui.selectable_value(&mut s.mode, Mode::Dither, t.mode_dither).on_hover_text(t.mode_dither_hint);
        });
        if s.mode == Mode::Threshold {
            ui.add(egui::Slider::new(&mut s.threshold, 1..=255).text(t.threshold));
        }
        ui.checkbox(&mut s.invert, t.invert).on_hover_text(t.invert_hint);
    }

    ui.add_space(6.0);
    ui.label(t.anchor).on_hover_text(t.anchor_hint);
    ui.horizontal(|ui| {
        if let Some(anchor) = anchor_grid(ui, p.anchor) {
            let b = picture_box(p, label);
            p.anchor = anchor;
            p.anchor_locked = true;
            place(p, b.l, b.t, label);
        }
        let mut auto = !p.anchor_locked;
        if ui.checkbox(&mut auto, t.anchor_auto).changed() {
            let b = picture_box(p, label);
            p.anchor_locked = !auto;
            place(p, b.l, b.t, label);
        }
    });
    ui.label(t.anchor_position);
    Grid::new("anchor-position").num_columns(4).spacing([6.0, 4.0]).show(ui, |ui| {
        ui.label("x");
        ui.add(DragValue::new(&mut p.x).range(-10.0..=11.0).speed(0.002).max_decimals(3));
        ui.label("y");
        ui.add(DragValue::new(&mut p.y).range(-10.0..=11.0).speed(0.002).max_decimals(3));
        ui.end_row();
    });

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui.button(t.rotate).clicked() {
            rotate_picture(p);
        }
        if p.pending.is_some() && ui.button(RichText::new(t.bake).strong()).clicked() {
            // Baking freezes the bitmap: settle any conversion still running first.
            p.refresh_now();
            p.pending = None;
        }
    });
    if p.pending.is_some() {
        ui.weak(t.bake_hint);
    }
}

/// A 3×3 grid of the anchors; returns the one clicked.
fn anchor_grid(ui: &mut Ui, current: Anchor) -> Option<Anchor> {
    let (cell, gap) = (16.0, 3.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(3.0 * cell + 2.0 * gap), Sense::hover());
    ui.painter().rect_stroke(rect.shrink(cell / 2.0), 0.0, Stroke::new(1.0, ui.visuals().weak_text_color()), StrokeKind::Middle);
    let mut picked = None;
    for (i, &anchor) in Anchor::ALL.iter().enumerate() {
        let min = rect.min + Vec2::new((i % 3) as f32, (i / 3) as f32) * (cell + gap);
        let r = Rect::from_min_size(min, Vec2::splat(cell));
        let response = ui.interact(r, ui.id().with(("anchor", i)), Sense::click());
        let color = if anchor == current {
            ACCENT
        } else if response.hovered() {
            ui.visuals().widgets.hovered.bg_fill
        } else {
            ui.visuals().widgets.inactive.bg_fill
        };
        ui.painter().circle_filled(r.center(), cell * 0.35, color);
        if response.clicked() {
            picked = Some(anchor);
        }
    }
    picked
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

fn paint_grip(painter: &egui::Painter, pos: Pos2) {
    let grip = Rect::from_center_size(pos, Vec2::splat(7.0));
    painter.rect_filled(grip, 1.0, Color32::WHITE);
    painter.rect_stroke(grip, 1.0, Stroke::new(1.5, ACCENT), StrokeKind::Middle);
}

/// Corner grips of a picture being placed: it keeps its proportions, so there are no edge ones.
fn corner_grips(r: Rect) -> [(PictureGrab, Pos2, CursorIcon); 4] {
    let corner = |right, bottom| PictureGrab::Corner { right, bottom };
    [
        (corner(false, false), r.left_top(), CursorIcon::ResizeNwSe),
        (corner(true, false), r.right_top(), CursorIcon::ResizeNeSw),
        (corner(false, true), r.left_bottom(), CursorIcon::ResizeNeSw),
        (corner(true, true), r.right_bottom(), CursorIcon::ResizeNwSe),
    ]
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
        let fields = vec![Field { name: "F".into(), ..Field::default() }];
        Template { name: name.into(), orientation: None, fields, pictures: Vec::new() }
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

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::DynamicImage::new_luma8(w, h).write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
        bytes
    }

    /// The badge editor after one frame, so the canvas knows the label size, with a 200×100 picture added.
    fn with_picture(path: Option<&std::path::Path>) -> (Editor, egui::Context) {
        let (mut e, ctx) = badge_editor(path);
        press(&mut e, &ctx, Modifiers::NONE, Key::F20);
        e.add_picture(&png(200, 100), None);
        (e, ctx)
    }

    fn pictures(e: &Editor) -> &[Picture] {
        &e.draft[e.current().unwrap()].template.pictures
    }

    #[test]
    fn added_picture_fits_half_the_area_and_waits_to_be_baked() {
        let (e, _) = with_picture(None);
        let p = &pictures(&e)[0];
        let content_w = e.label_px.x - 2.0 * (MARGIN_MM * DPI as f32 / 25.4).round();
        assert_eq!(p.bitmap.width_px, (content_w / 2.0).round() as u32);
        assert_eq!(p.bitmap.height_px, p.bitmap.width_px / 2);
        assert_eq!((p.anchor, p.x, p.y), (Anchor::Center, 0.5, 0.5));
        assert!(p.pending.is_some() && !e.can_save());
        assert_eq!(e.picture, Some(0));
    }

    #[test]
    fn moving_picks_the_anchor_by_thirds_unless_locked() {
        let label = Vec2::new(400.0, 200.0);
        let mut p = Picture {
            x: 0.5,
            y: 0.5,
            anchor: Anchor::Center,
            anchor_locked: false,
            bitmap: itmo_labels_core::label::Bitmap::blank(0.0, 0.0, 40, 20),
            pending: None,
        };
        place(&mut p, 0.9, 0.9, label);
        assert_eq!(p.anchor, Anchor::BottomRight);
        assert!((p.x - 1.0).abs() < 1e-6 && (p.y - 1.0).abs() < 1e-6, "{p:?}");
        p.anchor_locked = true;
        place(&mut p, 0.0, 0.0, label);
        assert_eq!(p.anchor, Anchor::BottomRight, "locked");
        assert_eq!((p.x, p.y), (0.1, 0.1));
        let b = picture_box(&p, label);
        assert!(b.l.abs() < 1e-6 && b.t.abs() < 1e-6, "{b:?}");
    }

    #[test]
    fn corner_resize_keeps_the_opposite_corner_and_proportions() {
        let (mut e, _) = with_picture(None);
        let label = e.label_px;
        let ti = e.current().unwrap();
        let p = &mut e.draft[ti].template.pictures[0];
        let start = picture_box(p, label);
        let pointer = Pos2::new(start.r + 40.0 / label.x, start.b);
        resize_picture(p, start, true, true, pointer, label);
        p.refresh_now();
        let b = picture_box(p, label);
        assert!((b.l - start.l).abs() * label.x <= 1.0 && (b.t - start.t).abs() * label.y <= 1.0, "{start:?} → {b:?}");
        assert_eq!(p.bitmap.height_px, (p.bitmap.width_px as f32 / 2.0).round() as u32);
        assert!(p.bitmap.width_px > 0 && (b.r - start.r) * label.x > 30.0);
    }

    #[test]
    fn a_picture_can_hang_off_the_label_but_a_sliver_stays_on() {
        let mut p = Picture {
            x: 0.5,
            y: 0.5,
            anchor: Anchor::TopLeft,
            anchor_locked: true,
            bitmap: itmo_labels_core::label::Bitmap::blank(0.0, 0.0, 40, 20),
            pending: None,
        };
        let label = Vec2::new(400.0, 200.0);
        place(&mut p, -0.2, 0.9, label);
        let b = picture_box(&p, label);
        assert!((b.l + 0.08).abs() < 1e-6 && (b.t - 0.9).abs() < 1e-6, "{b:?}");
        assert!(b.r > 0.0 && b.b <= 1.0, "a sliver stays on the label: {b:?}");
        place(&mut p, -5.0, 5.0, label);
        let b = picture_box(&p, label);
        assert!(b.r > 0.0 && b.t < 1.0, "cannot be pushed completely off: {b:?}");
    }

    #[test]
    fn baking_can_be_undone_until_saved() {
        let path = std::env::temp_dir().join(format!("itmo-labels-picture-test-{}.toml", std::process::id()));
        let (mut e, ctx) = with_picture(Some(&path));
        press(&mut e, &ctx, Modifiers::NONE, Key::F20);
        press(&mut e, &ctx, Modifiers::COMMAND, Key::S);
        assert!(!path.exists(), "not baked: nothing written");
        let ti = e.current().unwrap();
        e.draft[ti].template.pictures[0].pending = None;
        press(&mut e, &ctx, Modifiers::NONE, Key::F20);
        press(&mut e, &ctx, Modifiers::COMMAND, Key::Z);
        assert!(pictures(&e)[0].pending.is_some(), "undo brings the source back");
        press(&mut e, &ctx, Modifiers::COMMAND | Modifiers::SHIFT, Key::Z);
        press(&mut e, &ctx, Modifiers::COMMAND, Key::S);
        let written = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(written.contains("[[template.picture]]"));
        press(&mut e, &ctx, Modifiers::COMMAND, Key::Z);
        assert_eq!(pictures(&e).len(), 1, "the step before baking");
        assert!(pictures(&e)[0].pending.is_none(), "but the source is gone after the save");
    }

    #[test]
    fn tab_walks_into_pictures_and_delete_removes_them() {
        let (mut e, ctx) = with_picture(None);
        e.picture = None;
        e.field = Some(3);
        press(&mut e, &ctx, Modifiers::NONE, Key::Tab);
        assert_eq!((e.field, e.picture), (None, Some(0)));
        press(&mut e, &ctx, Modifiers::NONE, Key::Tab);
        assert_eq!((e.field, e.picture), (Some(0), None), "wraps around to the fields");
        e.field = None;
        e.picture = Some(0);
        press(&mut e, &ctx, Modifiers::NONE, Key::ArrowRight);
        assert!(pictures(&e)[0].x > 0.5);
        press(&mut e, &ctx, Modifiers::NONE, Key::Delete);
        assert!(pictures(&e).is_empty() && e.picture.is_none());
        assert_eq!(names(&e).len(), 4, "fields untouched");
    }

    #[test]
    fn turning_a_picture_swaps_its_sides() {
        let (mut e, _) = with_picture(None);
        let ti = e.current().unwrap();
        let p = &mut e.draft[ti].template.pictures[0];
        let (w, h) = (p.bitmap.width_px, p.bitmap.height_px);
        rotate_picture(p);
        assert_eq!((p.bitmap.width_px, p.bitmap.height_px), (h, w), "while placing");
        p.pending = None;
        rotate_picture(p);
        assert_eq!((p.bitmap.width_px, p.bitmap.height_px), (w, h), "baked");
    }
}
