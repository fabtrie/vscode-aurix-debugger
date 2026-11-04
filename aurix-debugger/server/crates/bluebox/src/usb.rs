//! The probe's pipes through WinUSB, the driver the iC5700 is installed with.
//!
//! winIDEA sends WinUSB's kernel IOCTLs directly. `winusb.dll`'s
//! `WinUsb_WritePipe`/`ReadPipe` wrap the same IOCTLs, so the bytes on the
//! wire are the same.
//!
//! The probe can be open in one program at a time. While winIDEA holds it,
//! opening fails with error 5 (access denied).

use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows_sys::Win32::Devices::Usb::{
    WinUsb_Free, WinUsb_Initialize, WinUsb_ReadPipe, WinUsb_SetPipePolicy, WinUsb_WritePipe,
    GUID_DEVINTERFACE_USB_DEVICE, PIPE_TRANSFER_TIMEOUT, SHORT_PACKET_TERMINATE, WINUSB_INTERFACE_HANDLE,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

use crate::frame::{PIPE_IN, PIPE_OUT, PIPE_OUT_BULK};
use crate::{Error, Link, Result, PID, VID};

/// Default per-transfer timeout. A memory read takes about 2 ms; the
/// bring-up's longest wait in the captures was well under a second.
pub const DEFAULT_TIMEOUT_MS: u32 = 3000;

fn last_error(op: &'static str) -> Error {
    Error::Usb { op, code: unsafe { GetLastError() } }
}

/// Device paths of every plugged-in iC5700, e.g.
/// `\\?\usb#vid_06f9&pid_5700#01abc#{a5dcbf10-...}`. The serial is the third part.
pub fn device_paths() -> Result<Vec<String>> {
    let needle = format!("vid_{VID:04x}&pid_{PID:04x}");
    let mut out = Vec::new();
    unsafe {
        let set = SetupDiGetClassDevsW(
            &GUID_DEVINTERFACE_USB_DEVICE,
            null(),
            null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        );
        if set == INVALID_HANDLE_VALUE as isize {
            return Err(last_error("SetupDiGetClassDevsW"));
        }
        for i in 0.. {
            let mut ifd: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            ifd.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(set, null(), &GUID_DEVINTERFACE_USB_DEVICE, i, &mut ifd) == 0 {
                break;
            }
            let mut need = 0u32;
            SetupDiGetDeviceInterfaceDetailW(set, &ifd, null_mut(), 0, &mut need, null_mut());
            if need == 0 {
                continue;
            }
            // u32 storage keeps the struct aligned.
            let mut buf = vec![0u32; (need as usize).div_ceil(4)];
            let det = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*det).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(set, &ifd, det, need, null_mut(), null_mut()) == 0 {
                continue;
            }
            let p = std::ptr::addr_of!((*det).DevicePath) as *const u16;
            let max = (need as usize - 4) / 2;
            let len = (0..max).take_while(|&k| *p.add(k) != 0).count();
            let path = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
            if path.to_ascii_lowercase().contains(&needle) {
                out.push(path);
            }
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    Ok(out)
}

/// The serial number in a device path (`01ABC` in `...#vid_06f9&pid_5700#01abc#{...}`).
pub fn serial_of(path: &str) -> Option<&str> {
    path.split('#').nth(2)
}

pub struct UsbLink {
    file: HANDLE,
    usb: WINUSB_INTERFACE_HANDLE,
    path: String,
}

// The handles are plain kernel handles, usable from any thread; the link is
// used by one thread at a time (`&mut self`).
unsafe impl Send for UsbLink {}

impl UsbLink {
    /// Opens the probe with this serial, or the only one plugged in.
    pub fn open(serial: Option<&str>) -> Result<UsbLink> {
        let paths = device_paths()?;
        let pick: Vec<&String> = match serial {
            Some(s) => paths.iter().filter(|p| serial_of(p).is_some_and(|x| x.eq_ignore_ascii_case(s))).collect(),
            None => paths.iter().collect(),
        };
        match pick.as_slice() {
            [one] => UsbLink::open_path(one),
            [] => Err(Error::NotFound(match serial {
                Some(s) => format!("serial {s} not among {paths:?}"),
                None => "no device with VID 06f9, PID 5700 is present".into(),
            })),
            _ => Err(Error::NotFound(format!("several probes, pick one by serial: {paths:?}"))),
        }
    }

    pub fn open_path(path: &str) -> Result<UsbLink> {
        let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let file = CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                null_mut(),
            );
            if file == INVALID_HANDLE_VALUE {
                return Err(last_error("CreateFileW"));
            }
            let mut usb: WINUSB_INTERFACE_HANDLE = null_mut();
            if WinUsb_Initialize(file, &mut usb) == 0 {
                let e = last_error("WinUsb_Initialize");
                CloseHandle(file);
                return Err(e);
            }
            let link = UsbLink { file, usb, path: path.to_string() };
            // As winIDEA does: a split frame's payload ends in a short packet.
            link.policy(PIPE_OUT_BULK, SHORT_PACKET_TERMINATE, &1u8 as *const u8 as *const c_void, 1)?;
            link.set_timeout(DEFAULT_TIMEOUT_MS)?;
            Ok(link)
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Per-transfer timeout on all three pipes; 0 waits forever.
    pub fn set_timeout(&self, ms: u32) -> Result<()> {
        for pipe in [PIPE_OUT, PIPE_OUT_BULK, PIPE_IN] {
            self.policy(pipe, PIPE_TRANSFER_TIMEOUT, &ms as *const u32 as *const c_void, 4)?;
        }
        Ok(())
    }

    fn policy(&self, pipe: u8, policy: u32, value: *const c_void, len: u32) -> Result<()> {
        if unsafe { WinUsb_SetPipePolicy(self.usb, pipe, policy, len, value) } == 0 {
            return Err(last_error("WinUsb_SetPipePolicy"));
        }
        Ok(())
    }
}

impl Link for UsbLink {
    fn write(&mut self, pipe: u8, data: &[u8]) -> Result<()> {
        let mut n = 0u32;
        if unsafe { WinUsb_WritePipe(self.usb, pipe, data.as_ptr(), data.len() as u32, &mut n, null()) } == 0 {
            return Err(last_error("WinUsb_WritePipe"));
        }
        if n as usize != data.len() {
            return Err(Error::Protocol(format!("pipe {pipe:#04x}: wrote {n} of {} bytes", data.len())));
        }
        Ok(())
    }

    fn read(&mut self, pipe: u8, cap: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; cap];
        let mut n = 0u32;
        if unsafe { WinUsb_ReadPipe(self.usb, pipe, buf.as_mut_ptr(), cap as u32, &mut n, null()) } == 0 {
            return Err(last_error("WinUsb_ReadPipe"));
        }
        buf.truncate(n as usize);
        Ok(buf)
    }
}

impl Drop for UsbLink {
    fn drop(&mut self) {
        unsafe {
            WinUsb_Free(self.usb);
            CloseHandle(self.file);
        }
    }
}
