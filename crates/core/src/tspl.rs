//! TSPL/TSPL2 encoder (TSC and the many clones that report `CMD:XPP,XL` or `CMD:TSPL`).

use std::{fmt::Write as _, io, time::Duration};

use crate::{
    label::{Bitmap, Element, Font, Label, Rect, Rotation, Text},
    transport::Transport,
};

#[derive(Debug)]
pub enum EncodeError {
    /// Built-in fonts only cover ASCII; such text has to be rasterised into a bitmap.
    NonAsciiText(String),
    BitmapSize { expected: usize, actual: usize },
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::NonAsciiText(s) => write!(f, "built-in fonts are ASCII-only: {s:?}"),
            Self::BitmapSize { expected, actual } => write!(f, "bitmap data is {actual} bytes, expected {expected}"),
        }
    }
}

impl std::error::Error for EncodeError {}

pub struct Tspl {
    pub dpi: u32,
}

impl Tspl {
    fn dots(&self, mm: f32) -> u32 {
        (mm * self.dpi as f32 / 25.4).round().max(0.0) as u32
    }

    pub fn encode(&self, label: &Label, copies: u32) -> Result<Vec<u8>, EncodeError> {
        let mut out = Vec::new();
        let line = |out: &mut Vec<u8>, s: &str| {
            out.extend_from_slice(s.as_bytes());
            out.extend_from_slice(b"\r\n");
        };
        line(&mut out, &format!("SIZE {} mm, {} mm", label.width_mm, label.height_mm));
        line(&mut out, &format!("GAP {} mm, 0 mm", label.gap_mm));
        line(&mut out, "DIRECTION 0");
        line(&mut out, "CLS");
        for el in &label.elements {
            match el {
                Element::Text(t) => line(&mut out, &self.text(t)?),
                Element::Rect(r) => line(&mut out, &self.rect(r)),
                Element::Bitmap(b) => self.bitmap(&mut out, b)?,
            }
        }
        line(&mut out, &format!("PRINT {}", copies.max(1)));
        Ok(out)
    }

    fn text(&self, t: &Text) -> Result<String, EncodeError> {
        if !t.content.is_ascii() {
            return Err(EncodeError::NonAsciiText(t.content.clone()));
        }
        let Font::Builtin { id, x_mul, y_mul } = t.font;
        let rot = match t.rotation {
            Rotation::R0 => 0,
            Rotation::R90 => 90,
            Rotation::R180 => 180,
            Rotation::R270 => 270,
        };
        // TSPL escapes a double quote inside a string as \["].
        let content = t.content.replace('"', "\\[\"]");
        Ok(format!(
            "TEXT {},{},\"{id}\",{rot},{x_mul},{y_mul},\"{content}\"",
            self.dots(t.x_mm),
            self.dots(t.y_mm)
        ))
    }

    fn rect(&self, r: &Rect) -> String {
        let (x, y) = (self.dots(r.x_mm), self.dots(r.y_mm));
        format!(
            "BOX {x},{y},{},{},{}",
            x + self.dots(r.width_mm),
            y + self.dots(r.height_mm),
            self.dots(r.thickness_mm).max(1)
        )
    }

    fn bitmap(&self, out: &mut Vec<u8>, b: &Bitmap) -> Result<(), EncodeError> {
        let expected = b.row_bytes() * b.height_px as usize;
        if b.data.len() != expected {
            return Err(EncodeError::BitmapSize { expected, actual: b.data.len() });
        }
        let mut header = String::new();
        let _ = write!(header, "BITMAP {},{},{},{},0,", self.dots(b.x_mm), self.dots(b.y_mm), b.row_bytes(), b.height_px);
        out.extend_from_slice(header.as_bytes());
        // In TSPL a 0 bit burns a dot, so invert our 1 = black convention.
        // Padding bits past width_px become 1 = white.
        let pad_mask = match b.width_px % 8 {
            0 => 0,
            n => 0xffu8 >> n,
        };
        for row in b.data.chunks(b.row_bytes()) {
            let (last, body) = row.split_last().expect("row_bytes > 0");
            out.extend(body.iter().map(|byte| !byte));
            out.push(!last | pad_mask);
        }
        out.extend_from_slice(b"\r\n");
        Ok(())
    }
}

/// Reply byte to the immediate command `<ESC>!?`.
#[derive(Debug, Clone, Copy)]
pub struct Status(pub u8);

impl Status {
    pub const QUERY: &'static [u8] = b"\x1b!?";

    /// Sends `QUERY` and waits up to `quiet` for the reply, asking once more if none comes.
    ///
    /// Under Windows `usbprint.sys` every other reply goes missing: the driver resets the
    /// host side of the bulk IN data toggle while the printer keeps its own, so the host
    /// takes the next packet for a retransmission and drops it. That packet still flips
    /// the printer's toggle, so the second query's reply gets through.
    pub fn query(p: &mut dyn Transport, quiet: Duration) -> io::Result<Option<Self>> {
        for _ in 0..2 {
            p.write(Self::QUERY)?;
            if let Some(&b) = p.read(quiet)?.first() {
                return Ok(Some(Self(b)));
            }
        }
        Ok(None)
    }

    pub fn is_ready(self) -> bool {
        self.0 == 0
    }

    pub fn problems(self) -> Vec<&'static str> {
        const BITS: [&str; 8] = [
            "head opened",
            "paper jam",
            "out of paper",
            "out of ribbon",
            "pause",
            "printing",
            "cover opened",
            "other error",
        ];
        BITS.iter().enumerate().filter(|(i, _)| self.0 & (1 << i) != 0).map(|(_, s)| *s).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, io, time::Duration};

    use super::Status;
    use crate::transport::Transport;

    /// Answers each read with the next canned reply; counts writes.
    struct Fake {
        replies: VecDeque<Vec<u8>>,
        writes: usize,
    }

    impl Transport for Fake {
        fn write(&mut self, _: &[u8]) -> io::Result<()> {
            self.writes += 1;
            Ok(())
        }

        fn read(&mut self, _: Duration) -> io::Result<Vec<u8>> {
            Ok(self.replies.pop_front().unwrap_or_default())
        }

        fn device_id(&mut self) -> io::Result<String> {
            Ok(String::new())
        }
    }

    fn query(replies: &[&[u8]]) -> (Option<u8>, usize) {
        let mut p = Fake { replies: replies.iter().map(|r| r.to_vec()).collect(), writes: 0 };
        let s = Status::query(&mut p, Duration::ZERO).unwrap();
        (s.map(|s| s.0), p.writes)
    }

    #[test]
    fn status_first_reply() {
        assert_eq!(query(&[b"\x00"]), (Some(0), 1));
    }

    #[test]
    fn status_dropped_reply_is_asked_again() {
        assert_eq!(query(&[b"", b"\x04"]), (Some(4), 2));
    }

    #[test]
    fn status_no_reply_gives_up_after_retry() {
        assert_eq!(query(&[]), (None, 2));
    }
}
