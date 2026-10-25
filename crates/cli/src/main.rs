//! Spike CLI: talk to a USB label printer from userspace, bypassing the OS print spooler.
//!
//! Usage:
//!   labelprobe list                   — printers found, with backend and IEEE 1284 Device ID
//!   labelprobe devid                  — IEEE 1284 Device ID; prints nothing on paper
//!   labelprobe status                 — TSPL `<ESC>!?` status byte, decoded
//!   labelprobe query <escaped bytes>  — send bytes to bulk OUT, dump what comes back on bulk IN
//!   labelprobe send <file>            — send a raw file (e.g. a TSPL job) to bulk OUT
//!   labelprobe test [--dry-run]       — build a test label from the model and print it (or dump the TSPL)
//!   labelprobe render <WxH> <portrait|landscape> <text> <out.pbm> — rasterise a text label to a PBM image

use std::{env, fs, process::ExitCode, time::Duration};

use itmo_labels_core::{
    fonts,
    label::{Bitmap, Element, Font, Label, Rect, Rotation, Text},
    render::{self, Align, Fit, LabelSpec, Orientation, TextField, Zone},
    transport::{self, Transport},
    tspl::{Status, Tspl},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// `\xNN`, `\r`, `\n`, `\e` (ESC), `\\` escapes so probes can be typed on the command line.
fn unescape(s: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut it = s.bytes();
    while let Some(b) = it.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match it.next() {
            Some(b'r') => out.push(b'\r'),
            Some(b'n') => out.push(b'\n'),
            Some(b'e') => out.push(0x1b),
            Some(b'\\') => out.push(b'\\'),
            Some(b'x') => {
                let hex: Vec<u8> = it.by_ref().take(2).collect();
                out.push(u8::from_str_radix(std::str::from_utf8(&hex)?, 16)?);
            }
            other => return Err(format!("bad escape \\{}", other.map_or('?', char::from)).into()),
        }
    }
    Ok(out)
}

fn hexdump(data: &[u8]) {
    for (i, row) in data.chunks(16).enumerate() {
        let hex: Vec<String> = row.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String =
            row.iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' }).collect();
        println!("{:04x}  {:<48} {ascii}", i * 16, hex.join(" "));
    }
}

fn list() -> Result<()> {
    for found in transport::discover()? {
        let id = found.open().and_then(|mut p| p.device_id());
        match id {
            Ok(id) => println!("{}\n  {id}", found.name),
            Err(e) => println!("{}\n  cannot open: {e}", found.name),
        }
    }
    Ok(())
}

fn query_status(p: &mut dyn Transport) -> Result<Status> {
    p.write(Status::QUERY)?;
    let reply = p.read(Duration::from_millis(500))?;
    Ok(Status(*reply.first().ok_or("no reply to <ESC>!?")?))
}

/// 50×70 mm label exercising every element kind: border to check alignment,
/// built-in font text, and a bitmap with a non-multiple-of-8 width to check bit order and padding.
fn test_label() -> Label {
    let text = |x_mm, y_mm, id, mul, content: &str| {
        Element::Text(Text {
            x_mm,
            y_mm,
            content: content.into(),
            font: Font::Builtin { id, x_mul: mul, y_mul: mul },
            rotation: Rotation::R0,
        })
    };
    // 13 px squares, 117 px wide: last byte of each row is padded.
    let checker = Bitmap::from_fn(4.0, 30.0, 117, 104, |x, y| (x / 13 + y / 13) % 2 == 0);
    // Solid triangle in the top-left corner of the bitmap area tells which way is up.
    let arrow = Bitmap::from_fn(25.0, 30.0, 96, 96, |x, y| x <= y);
    Label {
        width_mm: 50.0,
        height_mm: 70.0,
        gap_mm: 2.0,
        elements: vec![
            Element::Rect(Rect { x_mm: 1.0, y_mm: 1.0, width_mm: 48.0, height_mm: 68.0, thickness_mm: 0.4 }),
            text(3.0, 3.0, 3, 1, "TOP"),
            text(3.0, 9.0, 4, 1, "ITMO labels"),
            text(3.0, 17.0, 2, 1, "nusb -> TSPL, no CUPS"),
            text(3.0, 22.0, 2, 1, "quote: \"ok\""),
            Element::Bitmap(checker),
            Element::Bitmap(arrow),
            text(3.0, 62.0, 2, 1, "BOTTOM 50x70"),
        ],
    }
}

fn run(args: &[String]) -> Result<()> {
    let arg = |i: usize, what: &str| args.get(i).ok_or_else(|| format!("missing {what}"));
    match args.first().map(String::as_str) {
        Some("list") => list(),
        Some("devid") => {
            println!("{}", transport::open_first()?.device_id()?);
            Ok(())
        }
        Some("status") => {
            let s = query_status(transport::open_first()?.as_mut())?;
            println!("status {:#04x}: {}", s.0, if s.is_ready() { "ready".into() } else { s.problems().join(", ") });
            Ok(())
        }
        Some("query") => {
            let data = unescape(arg(1, "bytes")?)?;
            let mut p = transport::open_first()?;
            p.write(&data)?;
            let reply = p.read(Duration::from_millis(800))?;
            println!("{} bytes back", reply.len());
            hexdump(&reply);
            Ok(())
        }
        Some("send") => {
            let path = arg(1, "file")?;
            let data = fs::read(path).map_err(|e| format!("{path}: {e}"))?;
            transport::open_first()?.write(&data)?;
            println!("sent {} bytes", data.len());
            Ok(())
        }
        Some("test") => {
            let job = Tspl { dpi: 203 }.encode(&test_label(), 1)?;
            if args.get(1).is_some_and(|a| a == "--dry-run") {
                hexdump(&job);
                return Ok(());
            }
            let mut p = transport::open_first()?;
            let before = query_status(p.as_mut())?;
            if !before.is_ready() {
                return Err(format!("printer not ready: {}", before.problems().join(", ")).into());
            }
            p.write(&job)?;
            println!("sent {} bytes", job.len());
            Ok(())
        }
        Some("render") => {
            // render <WxH> <portrait|landscape> <text> <out.pbm>
            let (w, h) = arg(1, "size")?.split_once('x').ok_or("size is WxH in mm")?;
            let orientation = match arg(2, "orientation")?.as_str() {
                "portrait" => Orientation::Portrait,
                "landscape" => Orientation::Landscape,
                other => return Err(format!("unknown orientation {other}").into()),
            };
            let font = fonts::regular();
            let fields = [TextField {
                text: arg(3, "text")?,
                font: &font,
                zone: Zone::FULL,
                align: Align::Center,
                fit: Fit::Ink,
                max_pt: None,
                wrap: false,
            }];
            let r = render::render(&LabelSpec {
                width_mm: w.parse()?,
                height_mm: h.parse()?,
                gap_mm: 2.0,
                dpi: 203,
                orientation,
                fields: &fields,
            });
            let out = arg(4, "output")?;
            // PBM P4 rows are 1 bpp, MSB first, 1 = black: exactly our Bitmap layout.
            let mut pbm = format!("P4\n{} {}\n", r.preview.width_px, r.preview.height_px).into_bytes();
            pbm.extend_from_slice(&r.preview.data);
            fs::write(out, pbm)?;
            println!("{out}: {:.1} pt", r.sizes_pt[0].unwrap_or(0.0));
            Ok(())
        }
        _ => Err("usage: labelprobe list | devid | status | query <bytes> | send <file> | test [--dry-run] | render <WxH> <portrait|landscape> <text> <out.pbm>".into()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
