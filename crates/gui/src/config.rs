//! Files in the OS config directory (`~/Library/Application Support/itmo-labels`,
//! `%APPDATA%\itmo-labels`, `~/.config/itmo-labels`):
//! - `sizes.toml`, `templates.toml`: seeded from the built-in versions on first run,
//!   edited by the user, re-read whenever they change;
//! - `state.toml`: the last session, written by the app.

use std::{
    collections::{BTreeMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use itmo_labels_core::render::{Align, Fit};
use serde::{Deserialize, Serialize};

const DEFAULT_SIZES: &str = r#"# Label sizes for ITMO Labels. This file is re-read as soon as it is saved.
#
# width_mm  — across the print head (the edge that comes out of the printer first)
# height_mm — along the feed
# gap_mm    — gap between labels on the liner (0 for continuous tape)
# name      — optional, shown in the list instead of "W×H"
#
# 50×80 and 80×50 are the same label wound differently; portrait/landscape
# mean the same thing for both.

[[size]]
width_mm = 50
height_mm = 70
gap_mm = 2

[[size]]
width_mm = 58
height_mm = 40
gap_mm = 2

[[size]]
width_mm = 50
height_mm = 30
gap_mm = 2

[[size]]
width_mm = 40
height_mm = 30
gap_mm = 2

[[size]]
width_mm = 30
height_mm = 20
gap_mm = 2
"#;

/// Written on top of `templates.toml` every time, since the editor regenerates the whole file.
const TEMPLATES_HEADER: &str = r#"# Label templates for ITMO Labels. This file is re-read as soon as it is saved.
# The template editor in the app rewrites it: comments other than this header are not kept.
#
# [[template]]
# name        — shown in the list; also the key under which the app remembers its texts
# orientation — optional, "portrait" or "landscape": used the first time the template is picked
#
# [[template.field]] — a text, auto-sized to fit its zone
# name   — label of the input box
# x, y   — top-left corner of the zone, as fractions (0..1) of the label inside its 2 mm margins
# w, h   — size of the zone, same units; default 1
# align  — "left", "center" (default) or "right"
# bold   — true or false (default)
# max_pt — optional upper limit on the font size, in points
# fit    — "line" (default): centre by the font's ascent/descent, so the baseline does not jump
#          between texts; "ink": centre by the letters actually drawn, for the largest text
# wrap   — true: break the text into lines at spaces when that makes it larger; false (default):
#          always one line
# sample — optional example text: the editor previews it, the input box shows it greyed out
"#;

const DEFAULT_TEMPLATES: &str = r#"[[template]]
name = "Plain text"

  [[template.field]]
  name = "Text"
  fit = "ink"

[[template]]
name = "Badge"
orientation = "landscape"

  [[template.field]]
  name = "Team number"
  h = 0.18
  align = "right"

  [[template.field]]
  name = "Name"
  y = 0.2
  h = 0.48
  wrap = true

  [[template.field]]
  name = "Team"
  y = 0.7
  h = 0.14

  [[template.field]]
  name = "University"
  y = 0.86
  h = 0.14
"#;

pub fn dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("itmo-labels")
}

/// A user-editable TOML file, seeded with a default and polled for changes by modification time.
pub struct WatchedFile<T> {
    pub path: PathBuf,
    parse: fn(&str) -> Result<T, String>,
    modified: Option<SystemTime>,
    checked: Instant,
}

impl<T> WatchedFile<T> {
    const POLL: Duration = Duration::from_secs(1);

    fn open(dir: &Path, name: &str, default: &str, parse: fn(&str) -> Result<T, String>) -> io::Result<Self> {
        let path = dir.join(name);
        if !path.exists() {
            fs::create_dir_all(dir)?;
            fs::write(&path, default)?;
        }
        Ok(Self { path, parse, modified: None, checked: Instant::now() - Self::POLL })
    }

    /// The new contents (or why they are broken) when the file changed since the last call.
    pub fn poll(&mut self) -> Option<Result<T, String>> {
        if self.checked.elapsed() < Self::POLL {
            return None;
        }
        self.checked = Instant::now();
        let modified = fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        if modified.is_some() && modified == self.modified {
            return None;
        }
        self.modified = modified;
        Some(fs::read_to_string(&self.path).map_err(|e| e.to_string()).and_then(|text| (self.parse)(&text)))
    }
}

// ---- sizes.toml

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Size {
    pub name: Option<String>,
    pub width_mm: f32,
    pub height_mm: f32,
    #[serde(default)]
    pub gap_mm: f32,
}

impl Size {
    /// Shown in the list and used as the key in `state.toml`.
    pub fn title(&self) -> String {
        self.name.clone().unwrap_or_else(|| format!("{}×{} mm", self.width_mm, self.height_mm))
    }
}

#[derive(Deserialize)]
struct SizesFile {
    #[serde(default, rename = "size")]
    sizes: Vec<Size>,
}

fn parse_sizes(text: &str) -> Result<Vec<Size>, String> {
    let file: SizesFile = toml::from_str(text).map_err(|e| e.to_string())?;
    if file.sizes.is_empty() {
        return Err("no [[size]] entries".into());
    }
    if let Some(bad) = file.sizes.iter().find(|s| !(s.width_mm > 0.0 && s.height_mm > 0.0 && s.gap_mm >= 0.0)) {
        return Err(format!("{}: sizes must be positive", bad.title()));
    }
    Ok(file.sizes)
}

pub fn sizes_file(dir: &Path) -> io::Result<WatchedFile<Vec<Size>>> {
    WatchedFile::open(dir, "sizes.toml", DEFAULT_SIZES, parse_sizes)
}

pub fn builtin_sizes() -> Vec<Size> {
    parse_sizes(DEFAULT_SIZES).expect("built-in sizes are valid")
}

// ---- templates.toml

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Orientation {
    #[default]
    Portrait,
    Landscape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FieldAlign {
    Left,
    #[default]
    Center,
    Right,
}

impl From<FieldAlign> for Align {
    fn from(a: FieldAlign) -> Self {
        match a {
            FieldAlign::Left => Align::Left,
            FieldAlign::Center => Align::Center,
            FieldAlign::Right => Align::Right,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FieldFit {
    #[default]
    Line,
    Ink,
}

impl From<FieldFit> for Fit {
    fn from(f: FieldFit) -> Self {
        match f {
            FieldFit::Line => Fit::Line,
            FieldFit::Ink => Fit::Ink,
        }
    }
}

fn one() -> f32 {
    1.0
}

fn is_zero(v: &f32) -> bool {
    *v == 0.0
}

fn is_one(v: &f32) -> bool {
    *v == 1.0
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

/// Drag-edited values are written as 0.3, not 0.30000001.
fn rounded<S: serde::Serializer>(v: &f32, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_f32((v * 10_000.0).round() / 10_000.0)
}

fn rounded_opt<S: serde::Serializer>(v: &Option<f32>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(v) => rounded(v, s),
        None => s.serialize_none(),
    }
}

/// Defaults are left out when written, so a saved file reads like a hand-written one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sample: String,
    #[serde(default, skip_serializing_if = "is_zero", serialize_with = "rounded")]
    pub x: f32,
    #[serde(default, skip_serializing_if = "is_zero", serialize_with = "rounded")]
    pub y: f32,
    #[serde(default = "one", skip_serializing_if = "is_one", serialize_with = "rounded")]
    pub w: f32,
    #[serde(default = "one", skip_serializing_if = "is_one", serialize_with = "rounded")]
    pub h: f32,
    #[serde(default, skip_serializing_if = "is_default")]
    pub align: FieldAlign,
    #[serde(default, skip_serializing_if = "is_default")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "Option::is_none", serialize_with = "rounded_opt")]
    pub max_pt: Option<f32>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub fit: FieldFit,
    #[serde(default, skip_serializing_if = "is_default")]
    pub wrap: bool,
}

impl Default for Field {
    fn default() -> Self {
        Self {
            name: String::new(),
            sample: String::new(),
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            align: FieldAlign::default(),
            bold: false,
            max_pt: None,
            fit: FieldFit::default(),
            wrap: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orientation: Option<Orientation>,
    #[serde(default, rename = "field")]
    pub fields: Vec<Field>,
}

#[derive(Serialize, Deserialize)]
struct TemplatesFile {
    #[serde(default, rename = "template")]
    templates: Vec<Template>,
}

fn validate_field(f: &Field) -> Result<(), String> {
    let unit = 0.0..=1.0;
    let fits = |start: f32, len: f32| unit.contains(&start) && len > 0.0 && start + len <= 1.0 + 1e-4;
    if !fits(f.x, f.w) || !fits(f.y, f.h) {
        return Err(format!("field \"{}\": x, y, w, h must be fractions 0..1 inside the label", f.name));
    }
    if f.max_pt.is_some_and(|pt| pt <= 0.0) {
        return Err(format!("field \"{}\": max_pt must be positive", f.name));
    }
    Ok(())
}

/// An empty list and templates without fields are fine: the editor can get there one deletion at a time.
fn validate_templates(templates: &[Template]) -> Result<(), String> {
    let mut names = HashSet::new();
    for t in templates {
        if t.name.trim().is_empty() {
            return Err("a template has an empty name".into());
        }
        if !names.insert(&t.name) {
            return Err(format!("template \"{}\" is defined twice", t.name));
        }
        let mut field_names = HashSet::new();
        for f in &t.fields {
            if f.name.trim().is_empty() {
                return Err(format!("template \"{}\": a field has an empty name", t.name));
            }
            if !field_names.insert(&f.name) {
                return Err(format!("template \"{}\": field \"{}\" is defined twice", t.name, f.name));
            }
            validate_field(f).map_err(|e| format!("template \"{}\": {e}", t.name))?;
        }
    }
    Ok(())
}

fn parse_templates(text: &str) -> Result<Vec<Template>, String> {
    let file: TemplatesFile = toml::from_str(text).map_err(|e| e.to_string())?;
    validate_templates(&file.templates)?;
    Ok(file.templates)
}

fn format_templates(templates: &[Template]) -> Result<String, String> {
    validate_templates(templates)?;
    let body = toml::to_string(&TemplatesFile { templates: templates.to_vec() }).map_err(|e| e.to_string())?;
    Ok(format!("{TEMPLATES_HEADER}\n{body}"))
}

/// Replaces the whole file; the header with the format description is written anew.
pub fn write_templates(path: &Path, templates: &[Template]) -> Result<(), String> {
    fs::write(path, format_templates(templates)?).map_err(|e| e.to_string())
}

pub fn templates_file(dir: &Path) -> io::Result<WatchedFile<Vec<Template>>> {
    WatchedFile::open(dir, "templates.toml", &format!("{TEMPLATES_HEADER}\n{DEFAULT_TEMPLATES}"), parse_templates)
}

pub fn builtin_templates() -> Vec<Template> {
    parse_templates(DEFAULT_TEMPLATES).expect("built-in templates are valid")
}

// ---- state.toml

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Ru,
    En,
}

impl Lang {
    pub fn from_system() -> Self {
        match sys_locale::get_locale() {
            Some(l) if l.to_lowercase().starts_with("ru") => Self::Ru,
            _ => Self::En,
        }
    }
}

/// What the user last did with one template.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TemplateState {
    pub orientation: Option<Orientation>,
    /// Field name → text.
    pub fields: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub printer: Option<String>,
    pub size: Option<String>,
    pub template: Option<String>,
    pub lang: Option<Lang>,
    /// Template name → its last texts and orientation.
    pub templates: BTreeMap<String, TemplateState>,
}

impl State {
    fn path(dir: &Path) -> PathBuf {
        dir.join("state.toml")
    }

    /// Missing or unreadable state is not an error: start fresh.
    pub fn load(dir: &Path) -> Self {
        fs::read_to_string(Self::path(dir)).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        let text = toml::to_string(self).map_err(io::Error::other)?;
        fs::write(Self::path(dir), text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_sizes_parse() {
        let sizes = builtin_sizes();
        assert_eq!(sizes[0], Size { name: None, width_mm: 50.0, height_mm: 70.0, gap_mm: 2.0 });
        assert_eq!(sizes[0].title(), "50×70 mm");
    }

    #[test]
    fn broken_sizes_are_rejected() {
        assert!(parse_sizes("").is_err());
        assert!(parse_sizes("[[size]]\nwidth_mm = 0\nheight_mm = 10").is_err());
        assert!(parse_sizes("[[size]]\nwidth_mm = ").is_err());
    }

    #[test]
    fn builtin_templates_parse() {
        let t = builtin_templates();
        assert_eq!(t.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["Plain text", "Badge"]);
        let badge = &t[1];
        assert_eq!(badge.orientation, Some(Orientation::Landscape));
        assert_eq!(badge.fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["Team number", "Name", "Team", "University"]);
        assert_eq!(badge.fields[0].align, FieldAlign::Right);
        assert!(badge.fields.iter().all(|f| !f.bold));
        assert!(badge.fields[1].wrap && !badge.fields[2].wrap);
        assert_eq!((badge.fields[2].x, badge.fields[2].w), (0.0, 1.0), "defaults fill the width");
        assert_eq!(badge.fields[2].h, badge.fields[3].h, "team and university get the same room");
        assert_eq!(t[0].fields[0].fit, FieldFit::Ink);
    }

    #[test]
    fn broken_templates_are_rejected() {
        let field = |extra: &str| format!("[[template]]\nname = \"T\"\n[[template.field]]\nname = \"F\"\n{extra}");
        assert!(parse_templates(&field("")).is_ok());
        assert!(parse_templates("").unwrap().is_empty(), "no templates is a valid, empty list");
        assert!(parse_templates("[[template]]\nname = \"T\"").unwrap()[0].fields.is_empty(), "no fields is valid too");
        assert!(parse_templates("[[template]]\nname = \" \"").is_err(), "empty name");
        assert!(parse_templates(&field("").replace("name = \"F\"", "name = \"\"")).is_err(), "empty field name");
        assert!(parse_templates(&field("y = 0.8\nh = 0.5")).is_err(), "zone past the bottom");
        assert!(parse_templates(&field("w = 0")).is_err(), "empty zone");
        assert!(parse_templates(&field("max_pt = -1")).is_err());
        assert!(parse_templates(&field("align = \"middle\"")).is_err());
        assert!(parse_templates(&field("colour = \"red\"")).is_err(), "typos are reported");
        assert!(parse_templates(&format!("{}\n{}", field(""), field(""))).is_err(), "duplicate template");
        assert!(parse_templates(&format!("{}[[template.field]]\nname = \"F\"", field(""))).is_err(), "duplicate field");
    }

    #[test]
    fn written_templates_read_back_the_same() {
        let mut templates = builtin_templates();
        let f = &mut templates[1].fields[1];
        f.sample = "Иван \"Ваня\" Иванов".into();
        f.max_pt = Some(24.0);
        f.bold = true;
        f.x = 0.1 + 0.2;
        f.w = 0.5;
        let text = format_templates(&templates).unwrap();
        assert!(text.starts_with(TEMPLATES_HEADER));
        assert!(text.contains("x = 0.3\n"), "rounded:\n{text}");
        assert!(!text.contains("w = 1"), "defaults are left out:\n{text}");
        let mut back = parse_templates(&text).unwrap();
        back[1].fields[1].x = templates[1].fields[1].x;
        assert_eq!(back, templates);
    }

    #[test]
    fn invalid_templates_are_not_written() {
        let mut templates = builtin_templates();
        templates[1].name = templates[0].name.clone();
        assert!(format_templates(&templates).is_err());
    }

    #[test]
    fn default_file_parses_with_its_header() {
        let text = format!("{TEMPLATES_HEADER}\n{DEFAULT_TEMPLATES}");
        assert_eq!(parse_templates(&text).unwrap(), builtin_templates());
    }

    #[test]
    fn state_round_trips() {
        let mut s = State { template: Some("Badge".into()), lang: Some(Lang::Ru), ..State::default() };
        let badge = s.templates.entry("Badge".into()).or_default();
        badge.orientation = Some(Orientation::Landscape);
        badge.fields.insert("Name".into(), "Иван \"Ваня\" Иванов".into());
        assert_eq!(toml::from_str::<State>(&toml::to_string(&s).unwrap()).unwrap(), s);
    }

    #[test]
    fn state_from_previous_version_still_loads() {
        let old = "printer = \"usb 353d:1154 Printer\"\norientation = \"landscape\"\nbold = true\ntext = \"hi\"\n";
        let s: State = toml::from_str(old).unwrap();
        assert_eq!(s.printer.as_deref(), Some("usb 353d:1154 Printer"));
    }
}
