// SPDX-License-Identifier: MIT OR Apache-2.0
//! Operating-system USB byte I/O; no Epson commands.
pub use crate::device::{Backend, Device, nusb_location};
use crate::{Error, Result};
use std::time::Duration;
pub mod usb;
#[cfg(windows)]
pub mod windows;

/// Exclusive, movable connection carrying raw ESC/I bytes.
///
/// Reads may return a short packet, but must not exceed `size`. Timeouts bound
/// one I/O call; Windows usbscan rounds them up to whole seconds. Implementations
/// must release the device when dropped and must not issue scanner commands.
pub trait Transport: Send {
    fn read(&mut self, size: usize, timeout: Duration) -> Result<Vec<u8>>;
    fn write(&mut self, bytes: &[u8], timeout: Duration) -> Result<usize>;
}

pub fn open(device: &Device) -> Result<Box<dyn Transport>> {
    match device.backend {
        Backend::Nusb => Ok(Box::new(usb::NusbTransport::open(&device.location)?)),
        #[cfg(windows)]
        Backend::Usbscan => Ok(Box::new(windows::UsbscanTransport::open(&device.location)?)),
        _ => Err(Error::Driver(
            "Select a concrete available transport".into(),
        )),
    }
}
