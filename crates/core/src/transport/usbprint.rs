//! Windows `usbprint.sys`: the in-box USB printer class driver. Its device interface
//! (`GUID_DEVINTERFACE_USBPRINT`) is a file handle whose reads and writes go straight to
//! the bulk endpoints. No driver install, no admin rights.

use std::{io, mem, ptr, time::Duration};

use windows_sys::Win32::{
    Devices::DeviceAndDriverInstallation::{
        DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
        SetupDiGetDeviceInterfaceDetailW,
    },
    Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_IO_PENDING, ERROR_NO_MORE_ITEMS, GENERIC_READ, GENERIC_WRITE,
        GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    },
    Graphics::Printing::{GUID_DEVINTERFACE_USBPRINT, IOCTL_USBPRINT_GET_1284_ID},
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile, WriteFile,
    },
    System::{
        IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED},
        Threading::{CreateEventW, WaitForSingleObject},
    },
};

use super::{Found, Transport, parse_device_id};

const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);

/// NUL-terminated device interface paths, e.g. `\\?\usb#vid_353d&pid_1154#...#{28d78fad-...}`.
fn interface_paths() -> io::Result<Vec<Vec<u16>>> {
    let set = unsafe {
        SetupDiGetClassDevsW(&GUID_DEVINTERFACE_USBPRINT, ptr::null(), ptr::null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE)
    };
    if set == INVALID_HANDLE_VALUE as isize {
        return Err(io::Error::last_os_error());
    }
    let mut paths = Vec::new();
    for index in 0.. {
        let mut iface = SP_DEVICE_INTERFACE_DATA { cbSize: mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32, ..unsafe { mem::zeroed() } };
        if unsafe { SetupDiEnumDeviceInterfaces(set, ptr::null(), &GUID_DEVINTERFACE_USBPRINT, index, &mut iface) } == 0 {
            if unsafe { GetLastError() } != ERROR_NO_MORE_ITEMS {
                let e = io::Error::last_os_error();
                unsafe { SetupDiDestroyDeviceInfoList(set) };
                return Err(e);
            }
            break;
        }
        let mut size = 0u32;
        unsafe { SetupDiGetDeviceInterfaceDetailW(set, &iface, ptr::null_mut(), 0, &mut size, ptr::null_mut()) };
        if unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        // u64 storage keeps the struct aligned.
        let mut storage = vec![0u64; (size as usize).div_ceil(8)];
        let detail = storage.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        unsafe { (*detail).cbSize = mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 };
        if unsafe { SetupDiGetDeviceInterfaceDetailW(set, &iface, detail, size, ptr::null_mut(), ptr::null_mut()) } == 0 {
            continue;
        }
        let start = unsafe { ptr::addr_of!((*detail).DevicePath).cast::<u16>() };
        let max = (size as usize - mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath)) / 2;
        let chars = unsafe { std::slice::from_raw_parts(start, max) };
        let len = chars.iter().position(|&c| c == 0).unwrap_or(max);
        paths.push(chars[..len].iter().copied().chain([0]).collect());
    }
    unsafe { SetupDiDestroyDeviceInfoList(set) };
    Ok(paths)
}

pub fn discover() -> io::Result<Vec<Found>> {
    Ok(interface_paths()?
        .into_iter()
        .map(|path| Found {
            name: format!("usbprint {}", String::from_utf16_lossy(&path[..path.len() - 1])),
            open: Box::new(move || Ok(Box::new(UsbPrint::open(&path)?) as Box<dyn Transport>)),
        })
        .collect())
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

pub struct UsbPrint {
    file: Handle,
}

impl UsbPrint {
    fn open(path: &[u16]) -> io::Result<Self> {
        let h = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { file: Handle(h) })
    }

    /// Runs one overlapped operation; `None` if it was cancelled after `timeout`.
    /// Waits for the operation to finish in every case, so buffers borrowed by `start` stay valid.
    fn overlapped(&self, timeout: Duration, start: impl FnOnce(*mut OVERLAPPED) -> i32) -> io::Result<Option<u32>> {
        let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let event = Handle(event);
        let mut ov = OVERLAPPED { hEvent: event.0, ..Default::default() };
        if start(&mut ov) == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
            return Err(io::Error::last_os_error());
        }
        let ms = timeout.as_millis().min(u32::MAX as u128 - 1) as u32;
        let timed_out = unsafe { WaitForSingleObject(event.0, ms) } != WAIT_OBJECT_0;
        if timed_out {
            unsafe { CancelIoEx(self.file.0, &ov) };
        }
        let mut n = 0u32;
        if unsafe { GetOverlappedResult(self.file.0, &ov, &mut n, 1) } == 0 {
            let e = io::Error::last_os_error();
            // ERROR_OPERATION_ABORTED: our own cancellation.
            return if timed_out && e.raw_os_error() == Some(995) { Ok(None) } else { Err(e) };
        }
        Ok(Some(n))
    }
}

impl Transport for UsbPrint {
    fn write(&mut self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let chunk = &data[..data.len().min(64 * 1024)];
            let n = self
                .overlapped(WRITE_TIMEOUT, |ov| unsafe {
                    WriteFile(self.file.0, chunk.as_ptr(), chunk.len() as u32, ptr::null_mut(), ov)
                })?
                .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "printer did not accept data"))?;
            data = &data[n as usize..];
        }
        Ok(())
    }

    fn read(&mut self, quiet: Duration) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            let n = self.overlapped(quiet, |ov| unsafe {
                ReadFile(self.file.0, buf.as_mut_ptr(), buf.len() as u32, ptr::null_mut(), ov)
            })?;
            match n {
                Some(n) if n > 0 => out.extend_from_slice(&buf[..n as usize]),
                _ => return Ok(out),
            }
        }
    }

    fn device_id(&mut self) -> io::Result<String> {
        let mut buf = [0u8; 1024];
        let n = self
            .overlapped(CONTROL_TIMEOUT, |ov| unsafe {
                DeviceIoControl(
                    self.file.0,
                    IOCTL_USBPRINT_GET_1284_ID,
                    ptr::null(),
                    0,
                    buf.as_mut_ptr().cast(),
                    buf.len() as u32,
                    ptr::null_mut(),
                    ov,
                )
            })?
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "no Device ID reply"))?;
        Ok(parse_device_id(&buf[..n as usize]))
    }
}
