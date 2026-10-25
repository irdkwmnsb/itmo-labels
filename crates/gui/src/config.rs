//! Files in the OS config directory (`~/Library/Application Support/itmo-labels`,
//! `%APPDATA%\itmo-labels`, `~/.config/itmo-labels`):
//! - `sizes.toml`: label sizes, seeded from the built-in list on first run, edited by the user,
//!   re-read whenever it changes;
//! - `state.toml`: the last session, written by the app.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

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

pub fn dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("itmo-labels")
}

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

/// `sizes.toml`, polled for changes by modification time.
pub struct SizesFileWatch {
    pub path: PathBuf,
    modified: Option<SystemTime>,
    checked: Instant,
}

impl SizesFileWatch {
    const POLL: Duration = Duration::from_secs(1);

    /// Creates the file from the built-in list if it does not exist yet.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let path = dir.join("sizes.toml");
        if !path.exists() {
            fs::create_dir_all(dir)?;
            fs::write(&path, DEFAULT_SIZES)?;
        }
        Ok(Self { path, modified: None, checked: Instant::now() - Self::POLL })
    }

    /// The new list (or why it is broken) when the file changed since the last call.
    pub fn poll(&mut self) -> Option<Result<Vec<Size>, String>> {
        if self.checked.elapsed() < Self::POLL {
            return None;
        }
        self.checked = Instant::now();
        let modified = fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        if modified.is_some() && modified == self.modified {
            return None;
        }
        self.modified = modified;
        Some(fs::read_to_string(&self.path).map_err(|e| e.to_string()).and_then(|text| parse_sizes(&text)))
    }
}

pub fn builtin_sizes() -> Vec<Size> {
    parse_sizes(DEFAULT_SIZES).expect("built-in sizes are valid")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Orientation {
    #[default]
    Portrait,
    Landscape,
}

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub printer: Option<String>,
    pub size: Option<String>,
    pub orientation: Orientation,
    pub bold: bool,
    pub auto_size: bool,
    pub size_pt: f32,
    pub text: String,
    pub lang: Option<Lang>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            printer: None,
            size: None,
            orientation: Orientation::Portrait,
            bold: false,
            auto_size: true,
            size_pt: 24.0,
            text: String::new(),
            lang: None,
        }
    }
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
    fn state_round_trips() {
        let s = State { text: "Привет \"мир\"".into(), lang: Some(Lang::Ru), ..State::default() };
        assert_eq!(toml::from_str::<State>(&toml::to_string(&s).unwrap()).unwrap(), s);
    }
}
