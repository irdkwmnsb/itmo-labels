//! Host-side text rasterisation: any Unicode text in any TTF/OTF font becomes a 1-bit
//! bitmap in printer dots, so labels do not depend on the printer's built-in fonts.

use ab_glyph::{Font, FontRef, Glyph, PxScale, Rect, ScaleFont, point};

use crate::label::{Bitmap, Element, Label};

/// Blank border kept on every side of the label.
pub const MARGIN_MM: f32 = 2.0;
/// Coverage at or above which an anti-aliased pixel becomes a burnt dot.
const INK_THRESHOLD: f32 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Orientation {
    /// The line runs along the label's shorter side.
    #[default]
    Portrait,
    /// The line runs along the label's longer side.
    Landscape,
}

/// One line of text centred on a label.
pub struct TextLabel<'a> {
    /// Across the print head.
    pub width_mm: f32,
    /// Along the feed direction.
    pub height_mm: f32,
    pub gap_mm: f32,
    pub dpi: u32,
    pub orientation: Orientation,
    pub font: &'a FontRef<'a>,
    pub text: &'a str,
    /// `None` picks the largest size that fits inside the margins.
    pub size_pt: Option<f32>,
}

pub struct Rendered {
    /// Ready for an encoder: one full-label bitmap in printer orientation.
    pub label: Label,
    /// The same pixels as the reader sees them (rotated back if the line was turned).
    pub preview: Bitmap,
    /// Font size actually used.
    pub size_pt: f32,
    /// Ink crosses the margins (only possible with a manual size).
    pub overflows: bool,
}

fn dots(mm: f32, dpi: u32) -> u32 {
    (mm * dpi as f32 / 25.4).round().max(1.0) as u32
}

/// Lays the line out with its baseline at y = 0 and returns positioned glyphs plus their ink box.
fn layout(font: &FontRef, text: &str, scale: PxScale) -> (Vec<Glyph>, Option<Rect>) {
    let scaled = font.as_scaled(scale);
    let mut glyphs = Vec::new();
    let mut ink: Option<Rect> = None;
    let mut pen = 0.0;
    let mut prev = None;
    for c in text.chars() {
        let id = scaled.glyph_id(c);
        if let Some(prev) = prev {
            pen += scaled.kern(prev, id);
        }
        let glyph = id.with_scale_and_position(scale, point(pen, 0.0));
        pen += scaled.h_advance(id);
        prev = Some(id);
        if let Some(outline) = font.outline_glyph(glyph.clone()) {
            let b = outline.px_bounds();
            ink = Some(match ink {
                None => b,
                Some(r) => Rect {
                    min: point(r.min.x.min(b.min.x), r.min.y.min(b.min.y)),
                    max: point(r.max.x.max(b.max.x), r.max.y.max(b.max.y)),
                },
            });
        }
        glyphs.push(glyph);
    }
    (glyphs, ink)
}

/// `PxScale` is relative to ascent − descent, not to the em square.
fn scale_per_pt(font: &FontRef, dpi: u32) -> f32 {
    let units_per_em = font.units_per_em().unwrap_or(1000.0);
    dpi as f32 / 72.0 * font.height_unscaled() / units_per_em
}

pub fn render(spec: &TextLabel) -> Rendered {
    let (across, along) = (dots(spec.width_mm, spec.dpi), dots(spec.height_mm, spec.dpi));
    // The line runs across the head unless the chosen orientation wants the other side.
    let across_is_short = spec.width_mm <= spec.height_mm;
    let rotate = match spec.orientation {
        Orientation::Portrait => !across_is_short,
        Orientation::Landscape => across_is_short && spec.width_mm != spec.height_mm,
    };
    let (w, h) = if rotate { (along, across) } else { (across, along) };
    let margin = dots(MARGIN_MM, spec.dpi) as f32;
    let (avail_w, avail_h) = ((w as f32 - 2.0 * margin).max(1.0), (h as f32 - 2.0 * margin).max(1.0));

    let per_pt = scale_per_pt(spec.font, spec.dpi);
    let mut scale = match spec.size_pt {
        Some(pt) => pt * per_pt,
        None => {
            let probe = 100.0;
            match layout(spec.font, spec.text, PxScale::from(probe)).1 {
                Some(ink) => probe * (avail_w / ink.width()).min(avail_h / ink.height()),
                None => 12.0 * per_pt,
            }
        }
    };
    let (mut glyphs, mut ink) = layout(spec.font, spec.text, PxScale::from(scale));
    // Pixel rounding makes ink grow slightly faster than linearly; shrink until it fits.
    if spec.size_pt.is_none() {
        while let Some(r) = ink.filter(|r| (r.width() > avail_w || r.height() > avail_h) && scale > 1.0) {
            scale *= (avail_w / r.width()).min(avail_h / r.height()).min(0.99);
            (glyphs, ink) = layout(spec.font, spec.text, PxScale::from(scale));
        }
    }

    let mut canvas = Bitmap::blank(0.0, 0.0, w, h);
    let mut overflows = false;
    if let Some(ink) = ink {
        overflows = ink.width() > avail_w + 0.5 || ink.height() > avail_h + 0.5;
        // Centre the ink box, snapped to whole pixels so every glyph shifts identically.
        let dx = ((w as f32 - ink.width()) / 2.0 - ink.min.x).round();
        let dy = ((h as f32 - ink.height()) / 2.0 - ink.min.y).round();
        for glyph in glyphs {
            let Some(outline) = spec.font.outline_glyph(glyph) else { continue };
            let b = outline.px_bounds();
            outline.draw(|x, y, coverage| {
                let (px, py) = (b.min.x + dx + x as f32, b.min.y + dy + y as f32);
                if coverage >= INK_THRESHOLD && px >= 0.0 && py >= 0.0 && (px as u32) < w && (py as u32) < h {
                    canvas.set(px as u32, py as u32);
                }
            });
        }
    }

    let printed = if rotate { canvas.rotated_cw() } else { canvas.clone() };
    Rendered {
        label: Label {
            width_mm: spec.width_mm,
            height_mm: spec.height_mm,
            gap_mm: spec.gap_mm,
            elements: vec![Element::Bitmap(printed)],
        },
        preview: canvas,
        size_pt: scale / per_pt,
        overflows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fonts;

    fn spec<'a>(font: &'a FontRef<'a>, w: f32, h: f32, orientation: Orientation, size_pt: Option<f32>) -> TextLabel<'a> {
        TextLabel { width_mm: w, height_mm: h, gap_mm: 2.0, dpi: 203, orientation, font, text: "Привет, ITMO", size_pt }
    }

    fn printed(r: &Rendered) -> &Bitmap {
        match &r.label.elements[..] {
            [Element::Bitmap(b)] => b,
            _ => panic!("expected one bitmap"),
        }
    }

    fn ink_box(b: &Bitmap) -> (u32, u32, u32, u32) {
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
        for y in 0..b.height_px {
            for x in 0..b.width_px {
                if b.get(x, y) {
                    (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
                }
            }
        }
        (x0, y0, x1, y1)
    }

    #[test]
    fn printer_bitmap_always_matches_stock() {
        let font = fonts::regular();
        for (w, h) in [(50.0, 70.0), (70.0, 50.0)] {
            for o in [Orientation::Portrait, Orientation::Landscape] {
                let b = printed(&render(&spec(&font, w, h, o, None))).clone();
                assert_eq!((b.width_px, b.height_px), (dots(w, 203), dots(h, 203)), "{w}x{h} {o:?}");
            }
        }
    }

    #[test]
    fn same_physical_label_regardless_of_roll_direction() {
        // 50×70 and 70×50 are the same label wound differently: the reader sees the same thing.
        let font = fonts::regular();
        for o in [Orientation::Portrait, Orientation::Landscape] {
            let a = render(&spec(&font, 50.0, 70.0, o, None));
            let b = render(&spec(&font, 70.0, 50.0, o, None));
            assert_eq!(a.preview.data, b.preview.data, "{o:?}");
        }
    }

    #[test]
    fn portrait_reads_along_short_side() {
        let font = fonts::regular();
        let r = render(&spec(&font, 70.0, 50.0, Orientation::Portrait, None));
        assert!(r.preview.width_px < r.preview.height_px);
        let (x0, _, x1, _) = ink_box(&r.preview);
        assert!(x1 - x0 > r.preview.width_px / 2, "line should span most of the width");
    }

    #[test]
    fn auto_size_stays_inside_margins() {
        let font = fonts::regular();
        let r = render(&spec(&font, 50.0, 70.0, Orientation::Landscape, None));
        assert!(!r.overflows);
        let m = dots(MARGIN_MM, 203);
        let (x0, y0, x1, y1) = ink_box(&r.preview);
        assert!(x0 >= m && y0 >= m && x1 < r.preview.width_px - m && y1 < r.preview.height_px - m);
    }

    #[test]
    fn manual_size_overflow_is_reported() {
        let font = fonts::regular();
        assert!(render(&spec(&font, 50.0, 70.0, Orientation::Portrait, Some(200.0))).overflows);
        assert!(!render(&spec(&font, 50.0, 70.0, Orientation::Portrait, Some(8.0))).overflows);
    }

    #[test]
    fn rotation_is_clockwise() {
        let mut b = Bitmap::blank(0.0, 0.0, 3, 2);
        b.set(0, 0);
        let r = b.rotated_cw();
        assert_eq!((r.width_px, r.height_px), (2, 3));
        assert!(r.get(1, 0), "top-left goes to top-right");
    }
}
