//! Linux `usblp`: the in-kernel USB printer class driver exposes the printer as `/dev/usb/lpN`.
//! Bytes go straight to the bulk endpoints; nothing to detach. Access needs the `lp` group.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use super::{Found, Transport, parse_device_id};

const SYSFS_CLASS: &str = "/sys/class/usbmisc";
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const DEVICE_ID_MAX: usize = 1024;
/// `_IOC(_IOC_READ, 'P', IOCNR_GET_DEVICE_ID, len)` from drivers/usb/class/usblp.c.
const LPIOC_GET_DEVICE_ID: u32 = (2 << 30) | ((DEVICE_ID_MAX as u32) << 16) | ((b'P' as u32) << 8) | 1;

fn sysfs_attr(dir: &Path, name: &str) -> String {
    fs::read_to_string(dir.join(name)).map(|s| s.trim().to_owned()).unwrap_or_default()
}

pub fn discover() -> io::Result<Vec<Found>> {
    let entries = match fs::read_dir(SYSFS_CLASS) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut found = Vec::new();
    for entry in entries {
        let node = entry?.file_name().to_string_lossy().into_owned();
        if !node.starts_with("lp") {
            continue;
        }
        // usbmisc/lpN/device is the USB interface; its parent is the USB device.
        let iface = fs::canonicalize(Path::new(SYSFS_CLASS).join(&node).join("device"))?;
        let dev = iface.parent().unwrap_or(&iface);
        let path = PathBuf::from("/dev/usb").join(&node);
        found.push(Found {
            name: format!(
                "usblp {} {}:{} {}",
                path.display(),
                sysfs_attr(dev, "idVendor"),
                sysfs_attr(dev, "idProduct"),
                sysfs_attr(dev, "product")
            ),
            open: Box::new(move || Ok(Box::new(Usblp::open(&path)?) as Box<dyn Transport>)),
        });
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

pub struct Usblp {
    file: File,
}

impl Usblp {
    pub fn open(path: &Path) -> io::Result<Self> {
        // Non-blocking so a stuck printer (no paper, open head) times out instead of hanging.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| match e.kind() {
                io::ErrorKind::PermissionDenied => io::Error::new(
                    e.kind(),
                    format!("{e}; add yourself to the lp group: sudo usermod -aG lp $USER, then log in again"),
                ),
                // usblp allows one opener at a time; CUPS may be printing.
                _ if e.raw_os_error() == Some(libc::EBUSY) => io::Error::new(e.kind(), format!("{e}; printer is in use")),
                _ => e,
            })?;
        Ok(Self { file })
    }

    /// Wait until `events` are ready; false on timeout.
    fn poll(&self, events: libc::c_short, timeout: Duration) -> io::Result<bool> {
        let mut pfd = libc::pollfd { fd: self.file.as_raw_fd(), events, revents: 0 };
        let ms = timeout.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
        loop {
            match unsafe { libc::poll(&mut pfd, 1, ms) } {
                -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
                -1 => return Err(io::Error::last_os_error()),
                0 => return Ok(false),
                _ if pfd.revents & libc::POLLHUP != 0 => return Err(io::ErrorKind::BrokenPipe.into()),
                _ => return Ok(true),
            }
        }
    }

    fn wait_writable(&self, deadline: Instant) -> io::Result<()> {
        let left = deadline.saturating_duration_since(Instant::now());
        if self.poll(libc::POLLOUT, left)? {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::TimedOut, "printer did not accept data"))
        }
    }
}

impl Transport for Usblp {
    fn write(&mut self, mut data: &[u8]) -> io::Result<()> {
        let deadline = Instant::now() + WRITE_TIMEOUT;
        while !data.is_empty() {
            match self.file.write(data) {
                Ok(n) => data = &data[n..],
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.wait_writable(deadline)?,
                Err(e) => return Err(e),
            }
        }
        // A non-blocking write returns once the last URB is submitted; closing now would cancel it.
        self.wait_writable(deadline)
    }

    fn read(&mut self, quiet: Duration) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 512];
        while self.poll(libc::POLLIN, quiet)? {
            match self.file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    fn device_id(&mut self) -> io::Result<String> {
        let mut buf = [0u8; DEVICE_ID_MAX];
        // Returns 0, not a length: the length is the reply's own big-endian prefix.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), LPIOC_GET_DEVICE_ID as _, buf.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(parse_device_id(&buf))
    }
}
