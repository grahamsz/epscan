// SPDX-License-Identifier: MIT
//! Exclusive connection lifecycle and Epson command execution.
pub mod esci;
pub mod image;
use crate::{
    Capabilities, Error, Result, Source,
    capabilities::scanner_model,
    device::{self, Backend, Device},
    protocol::hex,
    transport::{self, Transport},
};
use esci::Esci;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
pub(crate) fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

pub struct Session {
    pub device: Device,
    pub capabilities: Capabilities,
    legacy: Vec<u8>,
    extended: Vec<u8>,
    pub(crate) protocol: Option<Esci>,
}
impl Session {
    pub fn open(device: Device, timeout: Duration) -> Result<Self> {
        crate::capabilities::require_native_protocol(device.vid, device.pid)?;
        let transport = transport::open(&device)?;
        Self::with_transport(device, transport, timeout)
    }
    pub fn with_transport(
        device: Device,
        transport: Box<dyn Transport>,
        timeout: Duration,
    ) -> Result<Self> {
        crate::capabilities::require_native_protocol(device.vid, device.pid)?;
        let usb_model = scanner_model(device.vid, device.pid).ok_or_else(|| {
            Error::NotFound(format!(
                "Unsupported USB scanner {:04x}:{:04x}",
                device.vid, device.pid
            ))
        })?;
        let mut protocol = Esci::new(transport, timeout)?;
        let (legacy, extended, caps) = protocol.identify()?;
        usb_model.validate_identity(&caps)?;
        Ok(Self {
            device,
            capabilities: caps,
            legacy,
            extended,
            protocol: Some(protocol),
        })
    }
    pub fn connect(location: Option<&str>, backend: Backend, timeout: Duration) -> Result<Self> {
        let mut devices = device::list_devices(backend)?;
        if let Some(location) = location {
            devices.retain(|d| d.location == location);
        }
        if devices.is_empty() {
            return Err(Error::NotFound(
                "No matching supported Epson scanner; run list".into(),
            ));
        }
        if devices.len() != 1 {
            return Err(Error::Busy(
                "Multiple scanners; select a device location".into(),
            ));
        }
        Self::open(devices.remove(0), timeout)
    }
    pub fn close(&mut self) {
        self.protocol.take();
    }
    /// Whether this session still owns a usable connection.
    pub fn is_open(&self) -> bool {
        self.protocol.is_some()
    }
    pub(crate) fn protocol(&mut self) -> Result<&mut Esci> {
        self.protocol
            .as_mut()
            .ok_or_else(|| Error::Protocol("Session closed; reopen after failure".into()))
    }
    pub fn diagnostics(&mut self) -> Result<serde_json::Value> {
        let result = (|| {
            let status = self.protocol()?.status()?;
            let params = self.protocol()?.query(b"\x1cS", 64)?;
            Ok(
                serde_json::json!({"device":self.device,"capabilities":self.capabilities,"model_profile":self.capabilities.scanner_model()?,"status":status,
                "legacy_identity_hex":hex(&self.legacy),"extended_identity_hex":hex(&self.extended),
                "parameters_hex":hex(&params),"source_areas_mm":{
                    "flatbed":self.capabilities.area_mm(Source::Flatbed),"transparency":self.capabilities.area_mm(Source::Transparency),
                    "transparency-8x10":self.capabilities.area_mm(Source::Transparency8x10)},
                "source_optics": {
                    "transparency":self.capabilities.optics(Source::Transparency).ok(),
                    "transparency-8x10":self.capabilities.optics(Source::Transparency8x10).ok(),
                    "flatbed":self.capabilities.optics(Source::Flatbed).ok()
                },
                "infrared_validation":"experimental; 8-bit transfer observed; spectral and registration validation pending",
                "dpi_is_advertised_not_measured":true,"eject":false,"film_transport":false}),
            )
        })();
        if result.is_err() {
            self.close();
        }
        result
    }
}
