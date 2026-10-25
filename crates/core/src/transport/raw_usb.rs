//! Raw USB printer-class interface over nusb. Primary on macOS; fallback elsewhere.
//!
//! - macOS: the printer interface is not owned by a kernel driver, plain claim works.
//! - Linux: claims via usbfs, detaching any other driver; nusb reattaches it on drop.
//!   Needs write access to `/dev/bus/usb/BBB/DDD` (a udev rule with `TAG+="uaccess"`).
//! - Windows: only works if WinUSB is bound to the device.

use std::{io, time::Duration};

use nusb::{
    DeviceInfo, Interface, MaybeFuture,
    descriptors::TransferType,
    transfer::{Bulk, ControlIn, ControlType, Direction, In, Out, Recipient, TransferError},
};

use super::{Found, Transport, parse_device_id};

const USB_CLASS_PRINTER: u8 = 0x07;
const REQ_GET_DEVICE_ID: u8 = 0;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

fn is_printer(d: &DeviceInfo) -> bool {
    d.class() == USB_CLASS_PRINTER || d.interfaces().any(|i| i.class() == USB_CLASS_PRINTER)
}

pub fn discover() -> io::Result<Vec<Found>> {
    Ok(nusb::list_devices()
        .wait()?
        .filter(is_printer)
        .map(|info| Found {
            name: format!(
                "usb {:04x}:{:04x} {}",
                info.vendor_id(),
                info.product_id(),
                info.product_string().unwrap_or_default()
            ),
            open: Box::new(move || Ok(Box::new(RawUsb::open(&info)?) as Box<dyn Transport>)),
        })
        .collect())
}

pub struct RawUsb {
    iface: Interface,
    iface_num: u8,
    ep_out: u8,
    ep_in: Option<u8>,
}

impl RawUsb {
    pub fn open(info: &DeviceInfo) -> io::Result<Self> {
        let device = info.open().wait().map_err(with_access_hint)?;
        let config = device.active_configuration()?;

        let (iface_num, ep_out, ep_in) = config
            .interface_alt_settings()
            .filter(|alt| alt.class() == USB_CLASS_PRINTER && alt.alternate_setting() == 0)
            .find_map(|alt| {
                let bulk = |dir| {
                    alt.endpoints()
                        .find(|e| e.transfer_type() == TransferType::Bulk && e.direction() == dir)
                        .map(|e| e.address())
                };
                Some((alt.interface_number(), bulk(Direction::Out)?, bulk(Direction::In)))
            })
            .ok_or_else(|| io::Error::other("no printer interface with a bulk OUT endpoint"))?;

        #[cfg(target_os = "linux")]
        let iface = device.detach_and_claim_interface(iface_num).wait();
        #[cfg(not(target_os = "linux"))]
        let iface = device.claim_interface(iface_num).wait();
        let iface = iface.map_err(with_access_hint)?;

        Ok(Self { iface, iface_num, ep_out, ep_in })
    }
}

fn with_access_hint(e: nusb::Error) -> io::Error {
    let e = io::Error::from(e);
    if cfg!(target_os = "linux") && e.kind() == io::ErrorKind::PermissionDenied {
        return io::Error::new(e.kind(), format!("{e}; install the udev rule from docs/protocol.md"));
    }
    e
}

impl Transport for RawUsb {
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        let mut ep = self.iface.endpoint::<Bulk, Out>(self.ep_out)?;
        for chunk in data.chunks(16 * 1024) {
            ep.transfer_blocking(chunk.to_vec().into(), WRITE_TIMEOUT).status?;
        }
        Ok(())
    }

    fn read(&mut self, quiet: Duration) -> io::Result<Vec<u8>> {
        let addr = self.ep_in.ok_or_else(|| io::Error::other("printer has no bulk IN endpoint"))?;
        let mut ep = self.iface.endpoint::<Bulk, In>(addr)?;
        let mut out = Vec::new();
        loop {
            let buf = ep.allocate(ep.max_packet_size().max(64) * 8);
            let c = ep.transfer_blocking(buf, quiet);
            match c.status {
                Ok(()) => out.extend_from_slice(&c.buffer[..c.actual_len]),
                Err(TransferError::Cancelled) => return Ok(out),
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn device_id(&mut self) -> io::Result<String> {
        let req = ControlIn {
            control_type: ControlType::Class,
            recipient: Recipient::Interface,
            request: REQ_GET_DEVICE_ID,
            value: 0,
            // Interface in the high byte, alt setting in the low byte.
            index: u16::from(self.iface_num) << 8,
            length: 1024,
        };
        Ok(parse_device_id(&self.iface.control_in(req, CONTROL_TIMEOUT).wait()?))
    }
}
