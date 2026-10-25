//! Byte pipes to a USB printer, with nothing to install on any OS.
//!
//! Where the OS ships a USB printer class driver that exposes the device as a plain
//! read/write handle, we use it: nothing to detach, no root, no extra driver, and it
//! coexists with the spooler. Those drivers just pass bytes to the bulk endpoints.
//!
//! - macOS: no such driver binds the interface; raw USB via nusb (IOKit).
//! - Linux: `/dev/usb/lpN` from the in-kernel `usblp`; raw usbfs via nusb if usblp is absent.
//! - Windows: the `usbprint.sys` device interface; raw USB via nusb if WinUSB is bound instead.

use std::{io, time::Duration};

mod raw_usb;
#[cfg(target_os = "linux")]
mod usblp;
#[cfg(windows)]
mod usbprint;

pub trait Transport {
    fn write(&mut self, data: &[u8]) -> io::Result<()>;

    /// Everything the printer sends back until it stays silent for `quiet`.
    fn read(&mut self, quiet: Duration) -> io::Result<Vec<u8>>;

    /// IEEE 1284 Device ID, e.g. `MFG:AYIN;CMD:XPP,XL;MDL:AD230;...`. The `CMD:` field lists
    /// supported languages, which is how an encoder gets picked without printing anything.
    fn device_id(&mut self) -> io::Result<String>;
}

/// A printer present on this machine, not opened yet.
pub struct Found {
    /// Backend and device, e.g. `usblp /dev/usb/lp0 353d:1154 Printer`.
    pub name: String,
    open: Box<dyn Fn() -> io::Result<Box<dyn Transport>>>,
}

impl Found {
    pub fn open(&self) -> io::Result<Box<dyn Transport>> {
        (self.open)().map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", self.name)))
    }
}

#[cfg(target_os = "linux")]
fn discover_native() -> io::Result<Vec<Found>> {
    usblp::discover()
}

#[cfg(windows)]
fn discover_native() -> io::Result<Vec<Found>> {
    usbprint::discover()
}

#[cfg(not(any(target_os = "linux", windows)))]
fn discover_native() -> io::Result<Vec<Found>> {
    Ok(Vec::new())
}

/// The class driver's handles when the OS has any, raw USB otherwise. Never both:
/// grabbing raw USB under a busy class driver would cut into someone else's job.
pub fn discover() -> io::Result<Vec<Found>> {
    let native = discover_native()?;
    if !native.is_empty() {
        return Ok(native);
    }
    raw_usb::discover()
}

pub fn open_first() -> io::Result<Box<dyn Transport>> {
    discover()?
        .first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no USB printer found"))?
        .open()
}

/// The printer class GET_DEVICE_ID reply starts with a big-endian length that includes
/// itself; some OS drivers pass it through, some strip it. Device IDs are ASCII, so a
/// control byte up front means the prefix is there.
pub(crate) fn parse_device_id(raw: &[u8]) -> String {
    let body = match raw {
        [hi, lo, rest @ ..] if *hi < 0x20 => {
            let len = usize::from(u16::from_be_bytes([*hi, *lo])).saturating_sub(2);
            &rest[..len.min(rest.len())]
        }
        _ => raw,
    };
    String::from_utf8_lossy(body).trim_matches(|c: char| c.is_whitespace() || c == '\0').to_owned()
}

#[cfg(test)]
mod tests {
    use super::parse_device_id;

    #[test]
    fn device_id_with_length_prefix() {
        assert_eq!(parse_device_id(b"\x00\x0dMFG:AYIN;   junk"), "MFG:AYIN;");
    }

    #[test]
    fn device_id_without_length_prefix() {
        assert_eq!(parse_device_id(b"MFG:AYIN;CMD:XPP,XL;\0\0"), "MFG:AYIN;CMD:XPP,XL;");
    }

    #[test]
    fn device_id_length_past_buffer() {
        assert_eq!(parse_device_id(b"\x01\x00MFG:X;"), "MFG:X;");
    }
}
