// SPDX-License-Identifier: MIT OR Apache-2.0
//! Discover supported Epson scanners without opening them or changing drivers.
#[cfg(not(windows))]
use crate::Error;
use crate::transport::usb;
#[cfg(windows)]
use crate::transport::windows;
use crate::{Result, capabilities::scanner_model};
use nusb::MaybeFuture;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    #[default]
    Auto,
    Nusb,
    Usbscan,
}

#[derive(Clone, Debug, Serialize)]
pub struct Device {
    pub location: String,
    pub name: String,
    pub vid: u16,
    pub pid: u16,
    pub backend: Backend,
}

pub fn nusb_location(info: &nusb::DeviceInfo) -> String {
    format!("usb:{}:{}", info.bus_id(), info.device_address())
}

pub fn list_devices(backend: Backend) -> Result<Vec<Device>> {
    #[cfg(windows)]
    if backend != Backend::Nusb {
        let stock = windows::discover()?;
        if !stock.is_empty() || backend == Backend::Usbscan {
            return Ok(stock);
        }
    }
    #[cfg(not(windows))]
    if backend == Backend::Usbscan {
        return Err(Error::Driver("usbscan is Windows-only".into()));
    }
    Ok(nusb::list_devices()
        .wait()
        .map_err(usb::operation_error)?
        .filter_map(|d| {
            let model = scanner_model(d.vendor_id(), d.product_id())?;
            Some(Device {
                location: nusb_location(&d),
                name: d.product_string().unwrap_or(model.name).to_owned(),
                vid: d.vendor_id(),
                pid: d.product_id(),
                backend: Backend::Nusb,
            })
        })
        .collect())
}
