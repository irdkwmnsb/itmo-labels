//! Pictures in templates. A PNG or JPEG is turned into printer dots once, when it is baked
//! in the editor; from then on only that 1-bit bitmap exists, kept in `templates.toml`
//! as a base64 1-bit PNG. Until then the decoded file lives in memory only.

use std::{
    io::Cursor,
    sync::{Arc, Mutex},
};

use eframe::egui;
use web_time::{Duration, Instant};

use image::{DynamicImage, GrayAlphaImage, GrayImage, ImageDecoder, ImageFormat, ImageReader, Luma, imageops};
use itmo_labels_core::label::Bitmap;

/// The source is brought down to this longer side on import: 150 mm at 203 dpi,
/// far more than any label needs.
const SOURCE_MAX: u32 = 1200;

/// How soon after the last one a rate-limited conversion may run again.
const CONVERT_MIN_INTERVAL: Duration = Duration::from_millis(40);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Every dot by itself: crisp edges, for logos and line art.
    #[default]
    Threshold,
    /// Floyd–Steinberg: shades of grey as dot density, for photos.
    Dither,
}

/// How the source becomes dots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub width_px: u32,
    pub mode: Mode,
    /// Ink, 0..=255, from which a dot is burnt in `Threshold` mode.
    pub threshold: u8,
    /// Light parts print instead of dark ones: for a white logo on a transparent background.
    pub invert: bool,
}

/// A decoded file, shared between undo steps; equal only to itself.
#[derive(Debug, Clone)]
pub struct Source(Arc<GrayAlphaImage>);

impl PartialEq for Source {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Source {
    /// PNG or JPEG, turned upright by its EXIF orientation.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().map_err(|e| e.to_string())?;
        let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
        let orientation = decoder.orientation().map_err(|e| e.to_string())?;
        let mut image = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
        image.apply_orientation(orientation);
        if image.width().max(image.height()) > SOURCE_MAX {
            image = image.resize(SOURCE_MAX, SOURCE_MAX, imageops::FilterType::Triangle);
        }
        Ok(Self(Arc::new(image.to_luma_alpha8())))
    }

    pub fn size(&self) -> (u32, u32) {
        self.0.dimensions()
    }

    /// Stable identity, for caches keyed by the source.
    pub fn ptr(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }

    /// Height in dots at `width_px`, keeping the proportions.
    pub fn height_for(&self, width_px: u32) -> u32 {
        let (w, h) = self.size();
        (width_px as f32 * h as f32 / w as f32).round().max(1.0) as u32
    }

    pub fn rotated_cw(&self) -> Self {
        Self(Arc::new(imageops::rotate90(&*self.0)))
    }

    /// Ink coverage over white paper at the source's resolution: `dark * alpha`.
    /// Transparent parts never print; `invert` prints the light parts instead of the dark ones.
    pub fn flatten_ink(&self, invert: bool) -> GrayImage {
        GrayImage::from_fn(self.0.width(), self.0.height(), |x, y| {
            let [l, a] = self.0.get_pixel(x, y).0;
            let dark = if invert { l } else { 255 - l };
            Luma([(dark as u16 * a as u16 / 255) as u8])
        })
    }

    /// All at once, for one-off conversions: [`Self::flatten_ink`], resize, dots.
    #[cfg(test)]
    pub fn convert(&self, s: &Settings) -> Bitmap {
        convert(self, &self.flatten_ink(s.invert), s)
    }
}

/// Resizes the flattened ink to the asked width and turns it into dots.
fn convert(source: &Source, ink: &GrayImage, s: &Settings) -> Bitmap {
    let width = s.width_px.max(1);
    let height = source.height_for(width);
    let ink = imageops::resize(ink, width, height, imageops::FilterType::Triangle);
    match s.mode {
        Mode::Threshold => Bitmap::from_fn(0.0, 0.0, width, height, |x, y| ink.get_pixel(x, y).0[0] >= s.threshold.max(1)),
        Mode::Dither => {
            let dots = dither(&ink);
            Bitmap::from_fn(0.0, 0.0, width, height, |x, y| dots[(y * width + x) as usize])
        }
    }
}

/// Floyd–Steinberg: each dot's rounding error is pushed on to the neighbours not yet drawn.
fn dither(ink: &GrayImage) -> Vec<bool> {
    let (w, h) = (ink.width() as usize, ink.height() as usize);
    let mut level: Vec<f32> = ink.pixels().map(|p| p.0[0] as f32).collect();
    let mut dots = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let black = level[i] >= 128.0;
            dots[i] = black;
            let err = level[i] - if black { 255.0 } else { 0.0 };
            let mut push = |dx: isize, dy: usize, share: f32| {
                let nx = x as isize + dx;
                if nx >= 0 && (nx as usize) < w && y + dy < h {
                    level[(y + dy) * w + nx as usize] += err * share;
                }
            };
            push(1, 0, 7.0 / 16.0);
            push(-1, 1, 3.0 / 16.0);
            push(0, 1, 5.0 / 16.0);
            push(1, 1, 1.0 / 16.0);
        }
    }
    dots
}

/// A picture still being placed: its file and how it is converted right now.
///
/// The full-resolution ink is flattened once and shared between undo steps (`Arc`);
/// the bitmap next to it is rebuilt from that ink when the settings change, at most
/// every `CONVERT_MIN_INTERVAL` while they keep changing (a dragged slider or corner).
#[derive(Debug, Clone)]
pub struct Pending {
    pub source: Source,
    pub settings: Settings,
    /// What the bitmap next to it was made with.
    pub made: Settings,
    /// Flattened ink at the source's resolution; rebuilt only when `invert` changes.
    ink: Arc<GrayImage>,
    ink_invert: bool,
    converted_at: Instant,
}

impl PartialEq for Pending {
    fn eq(&self, other: &Self) -> bool {
        // `ink` is derived from the source and `invert`; `converted_at` does not change the look.
        self.source == other.source && self.settings == other.settings && self.made == other.made
    }
}

impl Pending {
    pub fn new(source: Source, settings: Settings) -> (Self, Bitmap) {
        let ink = Arc::new(source.flatten_ink(settings.invert));
        let bitmap = convert(&source, &ink, &settings);
        (Self { source, settings, made: settings, ink_invert: settings.invert, ink, converted_at: Instant::now() }, bitmap)
    }

    /// The bitmap for the current settings, refreshing the cached ink if `invert` changed.
    pub fn convert(&mut self) -> Bitmap {
        if self.ink_invert != self.settings.invert {
            self.ink = Arc::new(self.source.flatten_ink(self.settings.invert));
            self.ink_invert = self.settings.invert;
        }
        self.converted_at = Instant::now();
        convert(&self.source, &self.ink, &self.settings)
    }

    /// A quarter turn: the source, the cached ink and the width follow it.
    pub fn rotate(&mut self) {
        self.settings.width_px = self.source.height_for(self.settings.width_px.max(1)).max(1);
        self.source = self.source.rotated_cw();
        self.ink = Arc::new(imageops::rotate90(&*self.ink));
    }

    /// What the bitmap was made from: settings plus source identity, for cache versioning.
    pub fn version(&self) -> (Settings, usize) {
        (self.settings, self.source.ptr())
    }

    /// Whether a rate-limited refresh may run now.
    pub fn due(&self) -> bool {
        self.converted_at.elapsed() >= CONVERT_MIN_INTERVAL
    }
}

/// The "add picture" file dialog: native and blocking on the desktop, an `<input type=file>`
/// on the web, where the file arrives a few frames later.
#[derive(Default)]
pub struct FilePicker {
    picked: Arc<Mutex<Option<Picked>>>,
}

/// The file's contents, or why it could not be read.
type Picked = Result<Vec<u8>, String>;

const EXTENSIONS: [&str; 3] = ["png", "jpg", "jpeg"];

impl FilePicker {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open(&self, _ctx: &egui::Context) {
        if let Some(path) = rfd::FileDialog::new().add_filter("PNG, JPEG", &EXTENSIONS).pick_file() {
            *self.picked.lock().unwrap() = Some(std::fs::read(path).map_err(|e| e.to_string()));
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn open(&self, ctx: &egui::Context) {
        let (picked, ctx) = (self.picked.clone(), ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            if let Some(file) = rfd::AsyncFileDialog::new().add_filter("PNG, JPEG", &EXTENSIONS).pick_file().await {
                *picked.lock().unwrap() = Some(Ok(file.read().await));
                ctx.request_repaint();
            }
        });
    }

    /// The file picked since the last call, if any.
    pub fn take(&self) -> Option<Picked> {
        self.picked.lock().unwrap().take()
    }
}

/// 1-bit greyscale PNG; in PNG 1 is white, in `Bitmap` 1 is black.
pub fn encode_png(b: &Bitmap) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, b.width_px, b.height_px);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::One);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    let data: Vec<u8> = b.data.iter().map(|byte| !byte).collect();
    writer.write_image_data(&data).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(out)
}

/// Any PNG: what is darker than mid-grey over white paper becomes a dot.
pub fn decode_png(bytes: &[u8]) -> Result<Bitmap, String> {
    let image = image::load_from_memory_with_format(bytes, ImageFormat::Png).map_err(|e| e.to_string())?.to_luma_alpha8();
    if image.width() == 0 || image.height() == 0 {
        return Err("empty picture".into());
    }
    Ok(Bitmap::from_fn(0.0, 0.0, image.width(), image.height(), |x, y| {
        let [l, a] = image.get_pixel(x, y).0;
        (255 - l) as u16 * a as u16 / 255 >= 128
    }))
}

/// `#[serde(with)]` for a bitmap stored as a base64 PNG; whitespace inside is ignored.
pub mod base64_png {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use itmo_labels_core::label::Bitmap;
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _, ser::Error as _};

    pub fn serialize<S: Serializer>(b: &Bitmap, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(super::encode_png(b).map_err(S::Error::custom)?))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Bitmap, D::Error> {
        let text: String = String::deserialize(d)?.split_whitespace().collect();
        let bytes = STANDARD.decode(text).map_err(D::Error::custom)?;
        super::decode_png(&bytes).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_of(image: DynamicImage) -> Vec<u8> {
        let mut bytes = Vec::new();
        image.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png).unwrap();
        bytes
    }

    fn settings(width_px: u32) -> Settings {
        Settings { width_px, mode: Mode::Threshold, threshold: 128, invert: false }
    }

    fn ink(b: &Bitmap) -> usize {
        (0..b.height_px).flat_map(|y| (0..b.width_px).map(move |x| (x, y))).filter(|&(x, y)| b.get(x, y)).count()
    }

    #[test]
    fn png_round_trips_dot_for_dot() {
        let b = Bitmap::from_fn(0.0, 0.0, 13, 5, |x, y| (x + y) % 3 == 0);
        assert_eq!(decode_png(&encode_png(&b).unwrap()).unwrap(), b);
    }

    #[test]
    fn converts_to_the_asked_width_keeping_proportions() {
        let src = Source::decode(&png_of(DynamicImage::new_luma8(400, 200))).unwrap();
        let b = src.convert(&settings(100));
        assert_eq!((b.width_px, b.height_px), (100, 50));
        assert_eq!(ink(&b), 100 * 50, "black stays black");
    }

    #[test]
    fn transparent_is_paper_and_invert_prints_the_light_parts() {
        // Left half: opaque white; right half: transparent black.
        let image = image::RgbaImage::from_fn(20, 10, |x, _| if x < 10 { [255, 255, 255, 255].into() } else { [0, 0, 0, 0].into() });
        let src = Source::decode(&png_of(image.into())).unwrap();
        assert_eq!(ink(&src.convert(&settings(20))), 0);
        let inverted = src.convert(&Settings { invert: true, ..settings(20) });
        assert!(inverted.get(2, 5) && !inverted.get(17, 5), "white prints, transparent does not");
    }

    #[test]
    fn dither_turns_grey_into_half_the_dots() {
        let src = Source::decode(&png_of(DynamicImage::ImageLuma8(GrayImage::from_pixel(40, 40, Luma([127]))))).unwrap();
        let b = src.convert(&Settings { mode: Mode::Dither, ..settings(40) });
        let share = ink(&b) as f32 / (40.0 * 40.0);
        assert!((0.4..0.6).contains(&share), "{share}");
        assert_eq!(ink(&src.convert(&settings(40))), 40 * 40, "threshold: all or nothing");
    }

    #[test]
    fn rotation_swaps_the_sides() {
        let src = Source::decode(&png_of(DynamicImage::new_luma8(30, 10))).unwrap().rotated_cw();
        assert_eq!(src.size(), (10, 30));
    }

    #[test]
    fn pending_keeps_the_ink_and_converts_on_demand() {
        let src = Source::decode(&png_of(DynamicImage::new_luma8(50, 25))).unwrap();
        let (mut pending, bitmap) = Pending::new(src, settings(20));
        assert_eq!((bitmap.width_px, bitmap.height_px), (20, 10));
        assert_eq!(pending.made, pending.settings);
        pending.settings.width_px = 40;
        assert_ne!(pending.made, pending.settings, "stale until converted");
        let bitmap = pending.convert();
        assert_eq!((bitmap.width_px, bitmap.height_px), (40, 20));
        assert!(!pending.due(), "just converted");
        pending.settings.invert = true;
        pending.convert();
        pending.rotate();
        assert_eq!(pending.source.size(), (25, 50));
        let bitmap = pending.convert();
        assert_eq!((bitmap.width_px, bitmap.height_px), (20, 40), "the width followed the turn");
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(Source::decode(b"not a picture").is_err());
        assert!(decode_png(b"\x89PNG broken").is_err());
    }
}
