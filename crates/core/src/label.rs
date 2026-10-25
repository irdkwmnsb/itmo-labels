//! Printer-independent label model. Coordinates are millimetres from the top-left
//! corner of the label as it leaves the printer; encoders convert to dots.

#[derive(Debug, Clone)]
pub struct Label {
    /// Across the print head.
    pub width_mm: f32,
    /// Along the feed direction.
    pub height_mm: f32,
    /// Gap between labels on the liner; 0 for continuous media.
    pub gap_mm: f32,
    pub elements: Vec<Element>,
}

#[derive(Debug, Clone)]
pub enum Element {
    Text(Text),
    Rect(Rect),
    Bitmap(Bitmap),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    #[default]
    R0,
    R90,
    R180,
    R270,
}

#[derive(Debug, Clone)]
pub struct Text {
    pub x_mm: f32,
    pub y_mm: f32,
    pub content: String,
    pub font: Font,
    pub rotation: Rotation,
}

/// Text that uses the printer's built-in bitmap fonts: fast, but ASCII only.
/// Anything else (Cyrillic, TTF fonts) will go through host-side rasterisation into a `Bitmap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Font {
    /// Printer font number with integer magnification.
    Builtin { id: u8, x_mul: u8, y_mul: u8 },
}

#[derive(Debug, Clone)]
pub struct Rect {
    pub x_mm: f32,
    pub y_mm: f32,
    pub width_mm: f32,
    pub height_mm: f32,
    pub thickness_mm: f32,
}

/// 1 bit per pixel, rows padded to whole bytes, MSB is the leftmost pixel, 1 = black.
/// Pixels map 1:1 to printer dots.
#[derive(Debug, Clone)]
pub struct Bitmap {
    pub x_mm: f32,
    pub y_mm: f32,
    pub width_px: u32,
    pub height_px: u32,
    pub data: Vec<u8>,
}

impl Bitmap {
    pub fn row_bytes(&self) -> usize {
        self.width_px.div_ceil(8) as usize
    }

    pub fn blank(x_mm: f32, y_mm: f32, width_px: u32, height_px: u32) -> Self {
        let data = vec![0u8; width_px.div_ceil(8) as usize * height_px as usize];
        Self { x_mm, y_mm, width_px, height_px, data }
    }

    pub fn get(&self, x: u32, y: u32) -> bool {
        self.data[y as usize * self.row_bytes() + x as usize / 8] & (0x80 >> (x % 8)) != 0
    }

    pub fn set(&mut self, x: u32, y: u32) {
        let row_bytes = self.row_bytes();
        self.data[y as usize * row_bytes + x as usize / 8] |= 0x80 >> (x % 8);
    }

    /// Rotated 90° clockwise: the top row becomes the right column.
    pub fn rotated_cw(&self) -> Self {
        let mut out = Self::blank(self.x_mm, self.y_mm, self.height_px, self.width_px);
        for y in 0..self.height_px {
            for x in 0..self.width_px {
                if self.get(x, y) {
                    out.set(self.height_px - 1 - y, x);
                }
            }
        }
        out
    }

    pub fn from_fn(x_mm: f32, y_mm: f32, width_px: u32, height_px: u32, black: impl Fn(u32, u32) -> bool) -> Self {
        let row_bytes = width_px.div_ceil(8) as usize;
        let mut data = vec![0u8; row_bytes * height_px as usize];
        for y in 0..height_px {
            for x in 0..width_px {
                if black(x, y) {
                    data[y as usize * row_bytes + x as usize / 8] |= 0x80 >> (x % 8);
                }
            }
        }
        Self { x_mm, y_mm, width_px, height_px, data }
    }
}
