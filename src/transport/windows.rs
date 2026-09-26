// SPDX-License-Identifier: MIT OR Apache-2.0
//! Bulk access through the installed usbscan.sys driver, without WIA/STI/TWAIN.
use super::{Backend, Device, Transport};
use crate::{Error, Result, capabilities::scanner_model};
use std::{
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    time::Duration,
};
use windows_sys::{
    Win32::{
        Devices::DeviceAndDriverInstallation::*,
        Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{CreateFileW, OPEN_EXISTING, ReadFile, WriteFile},
        System::IO::DeviceIoControl,
    },
    core::GUID,
};

fn win_error(operation: &str) -> Error {
    let error = std::io::Error::last_os_error();
    let text = format!("{operation}: {error}");
    match error.raw_os_error() {
        Some(2 | 3 | 1167) => Error::NotFound(text),
        Some(32 | 33) => Error::Busy(text),
        Some(5) => Error::Driver(text),
        Some(121 | 1460) => Error::Timeout(text),
        _ => Error::Io(error),
    }
}

struct DeviceSet(HDEVINFO);
impl Drop for DeviceSet {
    fn drop(&mut self) {
        unsafe {
            SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

pub fn discover() -> Result<Vec<Device>> {
    let guid = GUID::from_u128(0x6bdd1fc6_810f_11d0_bec7_08002be2092f);
    // SAFETY: typed Win32 structures, sized output buffers, and handles owned here.
    unsafe {
        let raw = SetupDiGetClassDevsW(
            &guid,
            null(),
            null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        );
        if raw == -1 {
            return Err(win_error("enumerating imaging interfaces"));
        }
        let set = DeviceSet(raw);
        let mut devices = Vec::new();
        for index in 0.. {
            let mut interface: SP_DEVICE_INTERFACE_DATA = zeroed();
            interface.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(set.0, null(), &guid, index, &mut interface) == 0 {
                if GetLastError() == 259 {
                    break;
                }
                return Err(win_error("enumerating interface"));
            }
            let mut needed = 0;
            SetupDiGetDeviceInterfaceDetailW(
                set.0,
                &interface,
                null_mut(),
                0,
                &mut needed,
                null_mut(),
            );
            if needed < 6 {
                return Err(win_error("sizing device path"));
            }
            // u64 allocation provides alignment for SP_DEVICE_INTERFACE_DETAIL_DATA_W.
            let mut storage = vec![0u64; (needed as usize).div_ceil(8)];
            let detail = storage
                .as_mut_ptr()
                .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(
                set.0,
                &interface,
                detail,
                needed,
                null_mut(),
                null_mut(),
            ) == 0
            {
                return Err(win_error("reading imaging path"));
            }
            let chars = std::slice::from_raw_parts(
                (*detail).DevicePath.as_ptr(),
                (needed as usize - 4) / 2,
            );
            let len = chars
                .iter()
                .position(|v| *v == 0)
                .ok_or_else(|| Error::Driver("Unterminated device path".into()))?;
            let path = String::from_utf16_lossy(&chars[..len]);
            if let Some((vid, pid)) = usb_ids_from_path(&path) {
                let Some(model) = scanner_model(vid, pid) else {
                    continue;
                };
                devices.push(Device {
                    location: path,
                    name: model.name.into(),
                    vid,
                    pid,
                    backend: Backend::Usbscan,
                });
            }
        }
        Ok(devices)
    }
}

fn usb_ids_from_path(path: &str) -> Option<(u16, u16)> {
    let path = path.to_ascii_lowercase();
    let read_id = |marker: &str| {
        let start = path.find(marker)? + marker.len();
        let digits = path.get(start..start + 4)?;
        if path
            .as_bytes()
            .get(start + 4)
            .is_some_and(u8::is_ascii_hexdigit)
        {
            return None;
        }
        u16::from_str_radix(digits, 16).ok()
    };
    Some((read_id("vid_")?, read_id("pid_")?))
}

pub struct UsbscanTransport {
    handle: HANDLE,
    timeout: u32,
}
// SAFETY: this type uniquely owns the file handle and only issues synchronous
// operations through `&mut self`. Windows file handles are not thread-affine.
// Moving ownership between threads cannot overlap I/O or close with another
// operation; sharing concurrent access is deliberately not provided (`Sync`).
unsafe impl Send for UsbscanTransport {}
impl Drop for UsbscanTransport {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}
impl UsbscanTransport {
    pub fn open(path: &str) -> Result<Self> {
        if path.contains('\0') {
            return Err(Error::Invalid(
                "USB device path contains a NUL character".into(),
            ));
        }
        let name: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        // Exclusive share mode 0: ownership extends across other OS processes.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                0xc0000000,
                0,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(win_error("opening scanner; close other scanner clients"));
        }
        let mut transport = Self { handle, timeout: 0 };
        let descriptor = transport.ioctl(6, &[], 8)?;
        let ids = descriptor
            .get(..4)
            .ok_or_else(|| Error::Driver("Truncated USB device descriptor".into()))?;
        let vid = u16::from_le_bytes([ids[0], ids[1]]);
        let pid = u16::from_le_bytes([ids[2], ids[3]]);
        scanner_model(vid, pid)
            .ok_or_else(|| Error::Driver(format!("Unsupported USB scanner {vid:04x}:{pid:04x}")))?;
        let pipes = transport.ioctl(10, &[], 68)?;
        if pipes.len() < 4 {
            return Err(Error::Driver("Truncated USB pipes".into()));
        }
        let count = u32::from_le_bytes(pipes[..4].try_into().unwrap()) as usize;
        if count > 8 || pipes.len() < 4 + count * 8 {
            return Err(Error::Driver("Invalid USB pipe count".into()));
        }
        let (mut inbound, mut outbound) = (0, 0);
        for entry in pipes[4..4 + count * 8].as_chunks::<8>().0 {
            if u32::from_le_bytes(entry[4..8].try_into().unwrap()) == 2 {
                if entry[2] & 128 != 0 {
                    inbound += 1;
                } else {
                    outbound += 1;
                }
            }
        }
        if (inbound, outbound) != (1, 1) {
            return Err(Error::Driver("Expected one bulk IN and OUT pipe".into()));
        }
        transport.set_timeout(Duration::from_secs(10))?;
        Ok(transport)
    }
    fn ioctl(&mut self, index: u32, bytes: &[u8], size: usize) -> Result<Vec<u8>> {
        let input_length = u32::try_from(bytes.len())
            .map_err(|_| Error::Invalid("USB control input too large".into()))?;
        let output_length = u32::try_from(size)
            .map_err(|_| Error::Invalid("USB control output too large".into()))?;
        let mut output = vec![0u8; size];
        let mut count = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                0x80002000 + index * 4,
                if bytes.is_empty() {
                    null()
                } else {
                    bytes.as_ptr().cast()
                },
                input_length,
                if size == 0 {
                    null_mut()
                } else {
                    output.as_mut_ptr().cast()
                },
                output_length,
                &mut count,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(win_error("usbscan IOCTL"));
        }
        if count as usize > size {
            return Err(Error::Protocol("Oversized IOCTL result".into()));
        }
        output.truncate(count as usize);
        Ok(output)
    }
    fn set_timeout(&mut self, timeout: Duration) -> Result<()> {
        if timeout.is_zero() || timeout > Duration::from_secs(214) {
            return Err(Error::Invalid(
                "usbscan timeout must be >0 and <=214 seconds".into(),
            ));
        }
        // usbscan.sys accepts whole seconds, so subsecond deadlines round up.
        let secs = timeout.as_secs_f64().ceil().clamp(1., 214.) as u32;
        if secs != self.timeout {
            let values: Vec<_> = [secs; 3].into_iter().flat_map(u32::to_le_bytes).collect();
            self.ioctl(11, &values, 0)?;
            self.timeout = secs;
        }
        Ok(())
    }
}
impl Transport for UsbscanTransport {
    fn read(&mut self, size: usize, timeout: Duration) -> Result<Vec<u8>> {
        if size == 0 {
            return Ok(Vec::new());
        }
        self.set_timeout(timeout)?;
        let length =
            u32::try_from(size).map_err(|_| Error::Invalid("USB read too large".into()))?;
        let mut buffer = vec![0u8; size];
        let mut count = 0;
        if unsafe {
            ReadFile(
                self.handle,
                buffer.as_mut_ptr(),
                length,
                &mut count,
                null_mut(),
            )
        } == 0
        {
            return Err(win_error("reading scanner"));
        }
        if count > length {
            return Err(Error::Protocol("Oversized USB read result".into()));
        }
        buffer.truncate(count as usize);
        Ok(buffer)
    }
    fn write(&mut self, bytes: &[u8], timeout: Duration) -> Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        self.set_timeout(timeout)?;
        let length =
            u32::try_from(bytes.len()).map_err(|_| Error::Invalid("USB write too large".into()))?;
        let mut count = 0;
        if unsafe { WriteFile(self.handle, bytes.as_ptr(), length, &mut count, null_mut()) } == 0 {
            return Err(win_error("writing scanner"));
        }
        if count > length {
            return Err(Error::Protocol("Oversized USB write result".into()));
        }
        Ok(count as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::usb_ids_from_path;

    #[test]
    fn device_path_ids_are_case_insensitive_and_exact() {
        assert_eq!(
            usb_ids_from_path(r"\\?\USB#VID_04B8&PID_0151#scanner"),
            Some((0x04b8, 0x0151))
        );
        assert_eq!(usb_ids_from_path("vid_04b8&pid_01512"), None);
        assert_eq!(usb_ids_from_path("vid_04b8&pid_xyz1"), None);
        assert_eq!(usb_ids_from_path("vid_04b8"), None);
    }
}
