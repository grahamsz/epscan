// SPDX-License-Identifier: MIT OR Apache-2.0
use super::{Transport, nusb_location};
use crate::{Error, Result, capabilities::scanner_model};
use nusb::{
    Endpoint, Interface, MaybeFuture,
    transfer::{Buffer, Bulk, In, Out, TransferError},
};
use std::{collections::VecDeque, time::Duration};

pub fn operation_error(e: nusb::Error) -> Error {
    match e.kind() {
        nusb::ErrorKind::Busy => Error::Busy(e.to_string()),
        nusb::ErrorKind::Disconnected | nusb::ErrorKind::NotFound => Error::NotFound(e.to_string()),
        _ => {
            #[cfg(windows)]
            let hint = ". nusb requires WinUSB; use the usbscan backend for Epson's stock driver";
            #[cfg(not(windows))]
            let hint = ". Check USB device permissions and close other scanner clients";
            Error::Driver(format!("nusb: {e}{hint}"))
        }
    }
}
fn transfer_error(e: TransferError) -> Error {
    match e {
        TransferError::Cancelled => Error::Timeout("nusb bulk transfer expired".into()),
        TransferError::Disconnected => Error::NotFound("USB disconnected".into()),
        other => Error::Io(std::io::Error::other(format!(
            "nusb bulk transfer: {other}"
        ))),
    }
}
pub struct NusbTransport {
    input: Endpoint<Bulk, In>,
    output: Endpoint<Bulk, Out>,
    _interface: Interface,
    pending: VecDeque<u8>,
    packet_size: usize,
}
impl NusbTransport {
    pub fn open(location: &str) -> Result<Self> {
        let info = nusb::list_devices()
            .wait()
            .map_err(operation_error)?
            .find(|d| {
                scanner_model(d.vendor_id(), d.product_id()).is_some()
                    && nusb_location(d) == location
            })
            .ok_or_else(|| Error::NotFound(location.into()))?;
        let device = info.open().wait().map_err(operation_error)?;
        let configuration = device
            .active_configuration()
            .map_err(|e| Error::Driver(e.to_string()))?;
        let mut candidates = Vec::new();
        for group in configuration.interfaces() {
            for alt in group.alt_settings().filter(|a| a.alternate_setting() == 0) {
                let endpoints: Vec<_> = alt
                    .endpoints()
                    .filter(|e| e.transfer_type() == nusb::descriptors::TransferType::Bulk)
                    .collect();
                let inputs: Vec<_> = endpoints
                    .iter()
                    .filter(|e| e.address() & 0x80 != 0)
                    .collect();
                let outputs: Vec<_> = endpoints
                    .iter()
                    .filter(|e| e.address() & 0x80 == 0)
                    .collect();
                if inputs.len() == 1 && outputs.len() == 1 {
                    candidates.push((
                        alt.interface_number(),
                        inputs[0].address(),
                        outputs[0].address(),
                    ));
                }
            }
        }
        if candidates.len() != 1 {
            return Err(Error::Driver("Ambiguous USB bulk interface layout".into()));
        }
        let (number, input_address, output_address) = candidates[0];
        let interface = device
            .claim_interface(number)
            .wait()
            .map_err(operation_error)?;
        let input = interface
            .endpoint::<Bulk, In>(input_address)
            .map_err(operation_error)?;
        let output = interface
            .endpoint::<Bulk, Out>(output_address)
            .map_err(operation_error)?;
        let packet_size = input.max_packet_size();
        if packet_size == 0 {
            return Err(Error::Driver("Zero USB packet size".into()));
        }
        Ok(Self {
            input,
            output,
            _interface: interface,
            pending: VecDeque::new(),
            packet_size,
        })
    }
}
impl Transport for NusbTransport {
    fn read(&mut self, size: usize, timeout: Duration) -> Result<Vec<u8>> {
        if size == 0 {
            return Ok(Vec::new());
        }
        if !self.pending.is_empty() {
            return Ok(self.pending.drain(..size.min(self.pending.len())).collect());
        }
        // nusb IN buffers must be packet aligned. Preserve surplus bytes across protocol reads.
        let request = size
            .div_ceil(self.packet_size)
            .checked_mul(self.packet_size)
            .ok_or_else(|| Error::Invalid("USB read too large".into()))?;
        let buffer = self
            .input
            .transfer_blocking(Buffer::new(request), timeout)
            .into_result()
            .map_err(transfer_error)?;
        let count = size.min(buffer.len());
        self.pending.extend(&buffer[count..]);
        Ok(buffer[..count].to_vec())
    }
    fn write(&mut self, bytes: &[u8], timeout: Duration) -> Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let done = self.output.transfer_blocking(bytes.into(), timeout);
        let count = done.actual_len;
        done.status.map_err(transfer_error)?;
        Ok(count)
    }
}
