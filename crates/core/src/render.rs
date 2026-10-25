//! Host-side text rasterisation: any Unicode text in any TTF/OTF font becomes a 1-bit
//! bitmap in printer dots, so labels do not depend on the printer's built-in fonts.
//!
//! A label is a set of text fields, each auto-sized independently into its own zone.

use ab_glyph::{Font, FontRef, Glyph, PxScale, Rect, ScaleFont, point};

use crate::label::{Bitmap, Element, Label};

/// Blank border kept on every side of the label.
pub const MARGIN_MM: f32 = 2.0;
/// Coverage at or above which an anti-aliased pixel becomes a burnt dot.
const INK_THRESHOLD: f32 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Orientation {
    /// Lines run along the label's shorter side.
    #[default]
    Portrait,
    /// Lines run along the label's longer side.
    Landscape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    Left,
    #[default]
    Center,
    Right,
}

/// What counts as the line's height when fitting and centring it vertically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fit {
    /// Font ascent to descent: the baseline does not move with the letters,
    /// so "Ivanov" and "Rogov" sit on the same line.
    #[default]
    Line,
    /// The ink actually drawn: the largest possible text, but it shifts with the letters.
    Ink,
}

/// Fractions of the content area (the label in reading orientation minus the margins).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Zone {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Zone {
    pub const FULL: Self = Self { x: 0.0, y: 0.0, w: 1.0, h: 1.0 };
}

pub struct TextField<'a> {
    pub text: &'a str,
    pub font: &'a FontRef<'a>,
    pub zone: Zone,
    pub align: Align,
    pub fit: Fit,
    /// Upper bound for the auto-fitted size.
    pub max_pt: Option<f32>,
}

pub struct LabelSpec<'a> {
    /// Across the print head.
    pub width_mm: f32,
    /// Along the feed direction.
    pub height_mm: f32,
    pub gap_mm: f32,
    pub dpi: u32,
    pub orientation: Orientation,
    pub fields: &'a [TextField<'a>],
}

pub struct Rendered {
    /// Ready for an encoder: one full-label bitmap in printer orientation.
    pub label: Label,
    /// The same pixels as the reader sees them (rotated back if the lines were turned).
    pub preview: Bitmap,
    /// Size each field ended up with; `None` for empty fields.
    pub sizes_pt: Vec<Option<f32>>,
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

/// Draws one field into `canvas` within the pixel rectangle `zone`; returns the size used.
fn draw_field(canvas: &mut Bitmap, field: &TextField, zone: Rect, dpi: u32) -> Option<f32> {
    let (zw, zh) = (zone.width().max(1.0), zone.height().max(1.0));
    let per_pt = scale_per_pt(field.font, dpi);
    let probe = 100.0;
    let probe_ink = layout(field.font, field.text, PxScale::from(probe)).1?;
    // With `Line` the line box is exactly `scale` tall, since that is what PxScale measures.
    let height_per_scale = match field.fit {
        Fit::Line => 1.0,
        Fit::Ink => probe_ink.height() / probe,
    };
    let mut scale = (probe * zw / probe_ink.width()).min(zh / height_per_scale);
    if let Some(max_pt) = field.max_pt {
        scale = scale.min(max_pt * per_pt);
    }
    let (mut glyphs, mut ink) = layout(field.font, field.text, PxScale::from(scale));
    // Pixel rounding makes ink grow slightly faster than linearly; shrink until it fits.
    let too_big = |r: &Rect, scale: f32| {
        r.width() > zw || matches!(field.fit, Fit::Ink) && r.height() > zh || matches!(field.fit, Fit::Line) && scale > zh
    };
    while let Some(r) = ink.filter(|r| too_big(r, scale) && scale > 1.0) {
        scale *= (zw / r.width()).min(0.99);
        (glyphs, ink) = layout(field.font, field.text, PxScale::from(scale));
    }
    let ink = ink?;

    let dx = match field.align {
        Align::Left => zone.min.x - ink.min.x,
        Align::Center => zone.min.x + (zw - ink.width()) / 2.0 - ink.min.x,
        Align::Right => zone.max.x - ink.max.x,
    };
    let dy = match field.fit {
        // Baseline such that the ascent..descent box is centred in the zone.
        Fit::Line => zone.min.y + (zh - scale) / 2.0 + field.font.as_scaled(PxScale::from(scale)).ascent(),
        Fit::Ink => zone.min.y + (zh - ink.height()) / 2.0 - ink.min.y,
    };
    // Whole-pixel offsets so every glyph shifts identically.
    let (dx, dy) = (dx.round(), dy.round());
    let (w, h) = (canvas.width_px, canvas.height_px);
    for glyph in glyphs {
        let Some(outline) = field.font.outline_glyph(glyph) else { continue };
        let b = outline.px_bounds();
        outline.draw(|x, y, coverage| {
            let (px, py) = (b.min.x + dx + x as f32, b.min.y + dy + y as f32);
            if coverage >= INK_THRESHOLD && px >= 0.0 && py >= 0.0 && (px as u32) < w && (py as u32) < h {
                canvas.set(px as u32, py as u32);
            }
        });
    }
    Some(scale / per_pt)
}

pub fn render(spec: &LabelSpec) -> Rendered {
    let (across, along) = (dots(spec.width_mm, spec.dpi), dots(spec.height_mm, spec.dpi));
    // Lines run across the head unless the chosen orientation wants the other side.
    let across_is_short = spec.width_mm <= spec.height_mm;
    let rotate = match spec.orientation {
        Orientation::Portrait => !across_is_short,
        Orientation::Landscape => across_is_short && spec.width_mm != spec.height_mm,
    };
    let (w, h) = if rotate { (along, across) } else { (across, along) };
    let margin = dots(MARGIN_MM, spec.dpi) as f32;
    let (content_w, content_h) = ((w as f32 - 2.0 * margin).max(1.0), (h as f32 - 2.0 * margin).max(1.0));

    let mut canvas = Bitmap::blank(0.0, 0.0, w, h);
    let sizes_pt = spec
        .fields
        .iter()
        .map(|field| {
            let z = field.zone;
            let zone = Rect {
                min: point(margin + z.x * content_w, margin + z.y * content_h),
                max: point(margin + (z.x + z.w) * content_w, margin + (z.y + z.h) * content_h),
            };
            draw_field(&mut canvas, field, zone, spec.dpi)
        })
        .collect();

    let printed = if rotate { canvas.rotated_cw() } else { canvas.clone() };
    Rendered {
        label: Label {
            width_mm: spec.width_mm,
            height_mm: spec.height_mm,
            gap_mm: spec.gap_mm,
            elements: vec![Element::Bitmap(printed)],
        },
        preview: canvas,
        sizes_pt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fonts;

    fn field<'a>(font: &'a FontRef<'a>, text: &'a str, zone: Zone, fit: Fit) -> TextField<'a> {
        TextField { text, font, zone, align: Align::Center, fit, max_pt: None }
    }

    fn spec<'a>(fields: &'a [TextField<'a>], w: f32, h: f32, orientation: Orientation) -> LabelSpec<'a> {
        LabelSpec { width_mm: w, height_mm: h, gap_mm: 2.0, dpi: 203, orientation, fields }
    }

    fn printed(r: &Rendered) -> &Bitmap {
        match &r.label.elements[..] {
            [Element::Bitmap(b)] => b,
            _ => panic!("expected one bitmap"),
        }
    }

    /// Ink bounds as (x0, y0, x1, y1), inclusive, optionally limited to rows y_from..y_to.
    fn ink_box(b: &Bitmap, rows: std::ops::Range<u32>) -> Option<(u32, u32, u32, u32)> {
        let mut found = None;
        for y in rows.start..rows.end.min(b.height_px) {
            for x in 0..b.width_px {
                if b.get(x, y) {
                    let (x0, y0, x1, y1) = found.unwrap_or((x, y, x, y));
                    found = Some((x0.min(x), y0.min(y), x1.max(x), y1.max(y)));
                }
            }
        }
        found
    }

    #[test]
    fn printer_bitmap_always_matches_stock() {
        let font = fonts::regular();
        let fields = [field(&font, "Привет, ITMO", Zone::FULL, Fit::Ink)];
        for (w, h) in [(50.0, 70.0), (70.0, 50.0)] {
            for o in [Orientation::Portrait, Orientation::Landscape] {
                let b = printed(&render(&spec(&fields, w, h, o))).clone();
                assert_eq!((b.width_px, b.height_px), (dots(w, 203), dots(h, 203)), "{w}x{h} {o:?}");
            }
        }
    }

    #[test]
    fn same_physical_label_regardless_of_roll_direction() {
        // 50×70 and 70×50 are the same label wound differently: the reader sees the same thing.
        let font = fonts::regular();
        let fields = [field(&font, "Привет, ITMO", Zone::FULL, Fit::Ink)];
        for o in [Orientation::Portrait, Orientation::Landscape] {
            let a = render(&spec(&fields, 50.0, 70.0, o));
            let b = render(&spec(&fields, 70.0, 50.0, o));
            assert_eq!(a.preview.data, b.preview.data, "{o:?}");
        }
    }

    #[test]
    fn portrait_reads_along_short_side() {
        let font = fonts::regular();
        let fields = [field(&font, "Привет, ITMO", Zone::FULL, Fit::Ink)];
        let r = render(&spec(&fields, 70.0, 50.0, Orientation::Portrait));
        assert!(r.preview.width_px < r.preview.height_px);
        let (x0, _, x1, _) = ink_box(&r.preview, 0..u32::MAX).unwrap();
        assert!(x1 - x0 > r.preview.width_px / 2, "line should span most of the width");
    }

    #[test]
    fn fields_stay_inside_margins() {
        let font = fonts::regular();
        for fit in [Fit::Ink, Fit::Line] {
            let fields = [field(&font, "Йцукен gjpq", Zone::FULL, fit)];
            let r = render(&spec(&fields, 50.0, 70.0, Orientation::Landscape));
            let m = dots(MARGIN_MM, 203);
            let (x0, y0, x1, y1) = ink_box(&r.preview, 0..u32::MAX).unwrap();
            assert!(x0 >= m && y0 >= m && x1 < r.preview.width_px - m && y1 < r.preview.height_px - m, "{fit:?}");
        }
    }

    #[test]
    fn line_fit_keeps_baseline_for_different_names() {
        // Same zone, same width-limited size: letters without descenders end at the baseline.
        let font = fonts::regular();
        let zone = Zone { x: 0.0, y: 0.3, w: 1.0, h: 0.3 };
        let bottom = |text: &str| {
            let fields = [field(&font, text, zone, Fit::Line)];
            let r = render(&spec(&fields, 50.0, 70.0, Orientation::Landscape));
            ink_box(&r.preview, 0..u32::MAX).unwrap().3
        };
        assert_eq!(bottom("HHHHHH"), bottom("НННННН"));
    }

    #[test]
    fn each_field_sized_independently_in_its_zone() {
        let font = fonts::regular();
        let fields = [
            field(&font, "Иван Иванов", Zone { x: 0.0, y: 0.18, w: 1.0, h: 0.32 }, Fit::Line),
            field(&font, "QA", Zone { x: 0.0, y: 0.53, w: 1.0, h: 0.19 }, Fit::Line),
            field(&font, "", Zone { x: 0.0, y: 0.84, w: 1.0, h: 0.16 }, Fit::Line),
        ];
        let r = render(&spec(&fields, 50.0, 70.0, Orientation::Landscape));
        assert!(r.sizes_pt[0].is_some() && r.sizes_pt[1].is_some());
        assert_eq!(r.sizes_pt[2], None, "empty field draws nothing");
        // "QA" is short, so it is limited by its zone height rather than the width.
        let content_h = r.preview.height_px as f32 - 2.0 * dots(MARGIN_MM, 203) as f32;
        let qa = ink_box(&r.preview, (dots(MARGIN_MM, 203) + (0.53 * content_h) as u32)..r.preview.height_px).unwrap();
        assert!((qa.3 - qa.1) as f32 <= 0.19 * content_h + 1.0);
    }

    #[test]
    fn max_pt_caps_size() {
        let font = fonts::regular();
        let fields = [TextField { max_pt: Some(10.0), ..field(&font, "A", Zone::FULL, Fit::Line) }];
        let r = render(&spec(&fields, 50.0, 70.0, Orientation::Portrait));
        assert!(r.sizes_pt[0].unwrap() <= 10.0 + 1e-3);
    }

    #[test]
    fn align_moves_text_to_zone_edge() {
        let font = fonts::regular();
        let m = dots(MARGIN_MM, 203);
        let zone = Zone { x: 0.0, y: 0.0, w: 1.0, h: 0.2 };
        let left = [TextField { align: Align::Left, ..field(&font, "ab", zone, Fit::Line) }];
        let right = [TextField { align: Align::Right, ..field(&font, "ab", zone, Fit::Line) }];
        let l = render(&spec(&left, 50.0, 70.0, Orientation::Landscape));
        let r = render(&spec(&right, 50.0, 70.0, Orientation::Landscape));
        let (lx0, ..) = ink_box(&l.preview, 0..u32::MAX).unwrap();
        let (_, _, rx1, _) = ink_box(&r.preview, 0..u32::MAX).unwrap();
        assert!(lx0.abs_diff(m) <= 1, "left edge {lx0} vs margin {m}");
        assert!(rx1.abs_diff(r.preview.width_px - m - 1) <= 1, "right edge {rx1}");
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
