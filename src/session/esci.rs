// SPDX-License-Identifier: MIT
//! Host transactions against the wire contract in docs/protocol.md.
use super::now;
use crate::{
    Error, Result, capabilities::TransferQuirks, error::unsupported, protocol::*,
    transport::Transport,
};
use std::{
    fs::File,
    io::Write,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub struct Esci {
    transport: Box<dyn Transport>,
    pub io_timeout: Duration,
    pub trace: Option<File>,
}
impl Esci {
    pub fn new(transport: Box<dyn Transport>, io_timeout: Duration) -> Result<Self> {
        if io_timeout.is_zero() || io_timeout > Duration::from_secs(214) {
            return Err(Error::Invalid(
                "I/O timeout must be >0 and <=214 seconds".into(),
            ));
        }
        Ok(Self {
            transport,
            io_timeout,
            trace: None,
        })
    }
    fn log(&mut self, direction: &str, bytes: &[u8], image: bool) -> Result<()> {
        if let Some(file) = &mut self.trace {
            serde_json::to_writer(
                &mut *file,
                &serde_json::json!({"time":now(),"direction":direction,"length":bytes.len(),
                "hex":if image {None} else {Some(hex(bytes))},"image":image}),
            )?;
            file.write_all(b"\n")?;
            file.flush()?;
        }
        Ok(())
    }
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.write_until(bytes, deadline(self.io_timeout, "I/O")?)
    }
    fn write_until(&mut self, bytes: &[u8], deadline: Instant) -> Result<()> {
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .min(self.io_timeout);
        if remaining.is_zero() {
            return Err(Error::Timeout("Deadline before command write".into()));
        }
        let count = self.transport.write(bytes, remaining)?;
        self.log("out", bytes, false)?;
        if count != bytes.len() {
            return Err(Error::Protocol(format!(
                "Short write {count}/{}",
                bytes.len()
            )));
        }
        Ok(())
    }
    pub fn read_exact(&mut self, size: usize, image: bool) -> Result<Vec<u8>> {
        self.read_exact_until(size, image, deadline(self.io_timeout, "I/O")?)
    }
    fn read_exact_until(
        &mut self,
        size: usize,
        image: bool,
        response_deadline: Instant,
    ) -> Result<Vec<u8>> {
        let mut data = Vec::with_capacity(size);
        let deadline = response_deadline.min(deadline(self.io_timeout, "I/O")?);
        while data.len() < size {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                if !image && data == [0x15] {
                    return Err(unsupported("ESC/I command", "device NAK"));
                }
                return Err(Error::Timeout(format!(
                    "Read {}/{} bytes",
                    data.len(),
                    size
                )));
            }
            let chunk = match self.transport.read(size - data.len(), remaining) {
                Err(Error::Timeout(_)) if !image && data == [0x15] => {
                    return Err(unsupported("ESC/I command", "device NAK"));
                }
                Err(Error::Timeout(message)) => {
                    return Err(Error::Timeout(format!(
                        "Read {}/{} bytes: {message}",
                        data.len(),
                        size
                    )));
                }
                result => result?,
            };
            self.log("in", &chunk, image)?;
            if chunk.is_empty() && !image && data == [0x15] {
                return Err(unsupported("ESC/I command", "device NAK"));
            }
            if chunk.is_empty() || chunk.len() > size - data.len() {
                return Err(Error::Protocol(
                    "Empty/oversized read before end of response".into(),
                ));
            }
            data.extend(chunk);
        }
        // A short first USB packet is not by itself a NAK: parameter data can
        // start with 0x15 (for example, the low byte of a requested DPI).
        if !image && data == [0x15] {
            return Err(unsupported("ESC/I command", "device NAK"));
        }
        Ok(data)
    }
    pub fn query(&mut self, command: &[u8], length: usize) -> Result<Vec<u8>> {
        self.write(command)?;
        self.read_exact(length, false)
    }
    pub fn simple(&mut self, command: &[u8]) -> Result<()> {
        self.expect_ack(
            command,
            &format!(
                "{} ({} bytes)",
                hex(&command[..command.len().min(8)]),
                command.len()
            ),
        )
    }
    fn expect_ack(&mut self, bytes: &[u8], context: &str) -> Result<()> {
        let answer = self.query(bytes, 1).map_err(|error| match error {
            Error::Unsupported { reason, .. } => unsupported(context, reason),
            Error::Protocol(message) => Error::Protocol(format!("{context}: {message}")),
            Error::Timeout(message) => Error::Timeout(format!("{context}: {message}")),
            other => other,
        })?;
        if answer != [6] {
            return Err(Error::Protocol(format!(
                "Expected ACK 06 after {context}; got {}{}. Stopping without retry; if subsequent commands fail, power-cycle the scanner",
                hex(&answer),
                if answer == [2] {
                    " (STX, an unexpected data-header marker)"
                } else {
                    ""
                }
            )));
        }
        Ok(())
    }
    fn setting(&mut self, command: &[u8], body: &[u8], name: &str) -> Result<()> {
        self.expect_ack(command, &format!("{name} command"))?;
        self.expect_ack(body, &format!("{name} payload ({} bytes)", body.len()))
    }
    pub fn identify(&mut self) -> Result<(Vec<u8>, Vec<u8>, Capabilities)> {
        let header = self.query(b"\x1bI", 4)?;
        let size = u16::from_le_bytes([header[2], header[3]]) as usize;
        if header[0] != 2 || !(2..=4096).contains(&size) {
            return Err(Error::Protocol("Invalid ESC I header".into()));
        }
        let legacy = self.read_exact(size, false)?;
        let extended = self.query(b"\x1cI", 80)?;
        let caps = Capabilities::parse(&extended)?;
        Ok((legacy, extended, caps))
    }
    pub fn status(&mut self) -> Result<Status> {
        Status::parse(&self.query(b"\x1cF", 16)?)
    }
    pub fn configure(&mut self, settings: &ScanSettings, ir: bool) -> Result<([u8; 64], Vec<u8>)> {
        let params = settings.parameters(ir)?;
        let focus = settings.source.optics().focus_command_position;
        self.configure_parameters(settings, ir, params, focus)
    }
    /// Configure a pass using the selected scanner's validated source and optics.
    pub fn configure_with_capabilities(
        &mut self,
        settings: &ScanSettings,
        ir: bool,
        capabilities: &Capabilities,
    ) -> Result<([u8; 64], Vec<u8>)> {
        // Validate the complete pass before reset or any other hardware write.
        let params = settings.parameters_with_capabilities(capabilities, ir)?;
        let focus = capabilities.optics(settings.source)?.focus_command_position;
        self.configure_parameters(settings, ir, params, focus)
    }
    fn configure_parameters(
        &mut self,
        settings: &ScanSettings,
        ir: bool,
        params: [u8; 64],
        focus: u8,
    ) -> Result<([u8; 64], Vec<u8>)> {
        self.expect_ack(b"\x1b@", "ESC @ reset")?;
        if ir {
            let current = self.query(b"\x1cS", 64)?;
            self.setting(
                b"\x1b#",
                &infrared_token(&current)?,
                "ESC # infrared enable",
            )?;
        }
        self.setting(b"\x1cW", &params, "FS W scan parameters")?;
        self.setting(b"\x1bp", &[focus], "ESC p focus")?;
        // ESC @ retains uploaded tables. DeviceDefault selects the built-in
        // curve instead of those custom tables; it does not assert linearity.
        if matches!(settings.gamma, Gamma::IdentityLut) {
            for color in *b"RGB" {
                let table: Vec<_> = std::iter::once(color).chain(0u8..=255).collect();
                self.setting(
                    b"\x1bz",
                    &table,
                    &format!("ESC z {} identity gamma table", char::from(color)),
                )?;
            }
        }
        let actual = self.query(b"\x1cS", 64)?;
        // Include byte 38 (main-lamp mode), the last defined setting used here.
        if actual[..39] != params[..39] {
            return Err(Error::Protocol(format!(
                "Parameter readback mismatch: {}",
                hex(&actual)
            )));
        }
        Ok((params, actual))
    }
    pub fn wait_ready(&mut self, cancel: &AtomicBool, timeout: Duration) -> Result<()> {
        let deadline = deadline(timeout, "Warmup")?;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout("Warmup".into()));
            }
            self.write_until(b"\x1cF", deadline)?;
            let status = Status::parse(&self.read_exact_until(16, false, deadline)?)?;
            if status.fatal || status.transparency_error {
                return Err(Error::Protocol(format!("Scanner status {status:?}")));
            }
            if status.busy && !status.warming_up {
                return Err(Error::Busy("Scanner not ready".into()));
            }
            if !status.warming_up {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout("Warmup".into()));
            }
            std::thread::sleep(
                Duration::from_millis(200).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
    /// Perform exactly one start; all rejected starts remain errors.
    pub fn acquire(
        &mut self,
        expected: u64,
        sink: &mut dyn Write,
        cancel: &AtomicBool,
        timeout: Duration,
        progress: &mut dyn FnMut(u64, u64) -> bool,
    ) -> Result<TransferResult> {
        self.acquire_inner(expected, sink, cancel, timeout, progress, false)
    }

    /// Apply the registered model's bounded start-preparation policy.
    ///
    /// Recovery never reconfigures the scanner. Only an exact zero-length
    /// 0x92 reply followed by confirmed warmup and healthy readiness permits
    /// one further start, under the original pass deadline.
    pub fn acquire_with_policy(
        &mut self,
        expected: u64,
        sink: &mut dyn Write,
        cancel: &AtomicBool,
        timeout: Duration,
        progress: &mut dyn FnMut(u64, u64) -> bool,
        policy: &TransferQuirks,
    ) -> Result<TransferResult> {
        self.acquire_inner(
            expected,
            sink,
            cancel,
            timeout,
            progress,
            policy.allow_start_warmup_recovery,
        )
    }

    fn acquire_inner(
        &mut self,
        expected: u64,
        sink: &mut dyn Write,
        cancel: &AtomicBool,
        timeout: Duration,
        progress: &mut dyn FnMut(u64, u64) -> bool,
        allow_recovery: bool,
    ) -> Result<TransferResult> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let deadline = deadline(timeout, "Pass")?;
        if expected == 0 {
            return Err(Error::Invalid(
                "Expected image byte count must be positive".into(),
            ));
        }
        let (raw, header, recovery) =
            self.start_acquisition(expected, cancel, deadline, allow_recovery)?;
        let count = u64::from(header.full_blocks) + u64::from(header.tail_size > 0);
        let (mut done, mut statuses) = (0, Vec::new());
        for i in 0..count {
            let size = if i == u64::from(header.full_blocks) {
                header.tail_size
            } else {
                header.block_size
            } as usize;
            let block = self
                .read_exact_until(size + 1, true, deadline)
                .map_err(|error| {
                    let context = |message: String| {
                        format!(
                            "Image block {}/{} (completed payload {done}/{expected} bytes; expecting {} data+status bytes): {message}",
                            i + 1,
                            count,
                            size + 1
                        )
                    };
                    match error {
                        Error::Timeout(message) => Error::Timeout(context(message)),
                        Error::Protocol(message) => Error::Protocol(context(message)),
                        other => other,
                    }
                })?;
            let status = block[size];
            let final_block = i + 1 == count;
            // Retain complete received blocks, including a faulted block, in
            // the partial payload. Only healthy nonfinal blocks permit CAN.
            let stored = sink.write_all(&block[..size]);
            done += size as u64;
            statuses.push(status);
            let position = format!(
                "Image block {}/{} (received payload {done}/{expected} bytes; status {status:02x})",
                i + 1,
                count,
            );
            // Fatal/not-ready terminates the device's transfer immediately,
            // even when the advertised image has more blocks. It is no longer
            // an ACK/CAN boundary. Keep the received bytes for diagnosis, but
            // do not report this faulted block as successful scan progress.
            if status & 0xc0 != 0 {
                if let Err(error) = stored {
                    return Err(Error::Io(std::io::Error::new(
                        error.kind(),
                        format!(
                            "{position}: writing image payload failed: {error}; scanner also ended the transfer; no ACK or CAN sent"
                        ),
                    )));
                }
                let fault = match status & 0xc0 {
                    0x80 => "fatal error",
                    0x40 => "not-ready status",
                    _ => "fatal error and not-ready status",
                };
                return Err(Error::Protocol(format!(
                    "{position}: scanner ended the image transfer with {fault}; no ACK or CAN sent"
                )));
            }
            let continuing = stored.is_err() || status & 0x10 != 0 || progress(done, expected);
            let cancelled = cancel.load(Ordering::Relaxed);
            let stopped = cancelled || !continuing || status & 0x10 != 0;
            let expired = Instant::now() >= deadline;
            if stopped || expired || stored.is_err() {
                let cleanup = if !final_block {
                    self.simple(&[0x18])
                } else {
                    Ok(())
                }
                .err()
                .map(|error| format!("; cancellation handshake also failed: {error}"))
                .unwrap_or_default();
                // A cleanup failure must not replace the initiating failure.
                // The session owner closes every failed acquisition.
                if let Err(error) = stored {
                    return Err(Error::Io(std::io::Error::new(
                        error.kind(),
                        format!("{position}: writing image payload failed: {error}{cleanup}"),
                    )));
                }
                if expired {
                    return Err(Error::Timeout(format!(
                        "{position}: pass deadline expired{cleanup}"
                    )));
                }
                if !cleanup.is_empty() {
                    let reason = if status & 0x10 != 0 {
                        "scanner requested cancellation"
                    } else if cancelled {
                        "scan cancellation was requested"
                    } else {
                        "progress callback requested cancellation"
                    };
                    return Err(Error::Protocol(format!("{position}: {reason}{cleanup}")));
                }
                return Err(Error::Cancelled);
            }
            if !final_block {
                self.write_until(&[6], deadline)?;
            }
        }
        Ok(TransferResult {
            header_hex: hex(&raw),
            block_status: statuses,
            received_bytes: done,
            start_attempts: 1 + u8::from(recovery.is_some()),
            start_recovery: recovery,
        })
    }

    fn start_acquisition(
        &mut self,
        expected: u64,
        cancel: &AtomicBool,
        deadline: Instant,
        allow_recovery: bool,
    ) -> Result<(Vec<u8>, TransferHeader, Option<StartRecovery>)> {
        check_pass(cancel, deadline)?;
        self.write_until(b"\x1cG", deadline)?;
        let raw = self.read_exact_until(14, false, deadline)?;
        let rejection = match TransferHeader::parse(&raw, expected) {
            Ok(header) => return Ok((raw, header, None)),
            Err(error) => error,
        };
        // A complete zero-length reply establishes the next command boundary.
        // Never infer it from an error string, a status bit alone, or a partial
        // header. No image bytes have been requested or delivered at this point.
        const PREPARING: [u8; 14] = [2, 0x92, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        if !allow_recovery || raw != PREPARING {
            return Err(rejection);
        }
        let Some(recovery) = self.wait_after_start_rejection(&raw, cancel, deadline)? else {
            return Err(rejection);
        };
        check_pass(cancel, deadline)?;
        log::info!("Scanner preparation finished; issuing the single resumed scan start");
        let (resumed, header) = (|| {
            self.write_until(b"\x1cG", deadline)?;
            let resumed = self.read_exact_until(14, false, deadline)?;
            // Deliberately no recursion or loop: every second rejection is terminal.
            let header = TransferHeader::parse(&resumed, expected)?;
            Ok((resumed, header))
        })()
        .map_err(resumed_start_error)?;
        Ok((resumed, header, Some(recovery)))
    }

    fn wait_after_start_rejection(
        &mut self,
        rejected: &[u8],
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<Option<StartRecovery>> {
        let started = Instant::now();
        let mut observations = Vec::new();
        loop {
            check_pass(cancel, deadline)?;
            self.write_until(b"\x1cF", deadline)?;
            let status = Status::parse(&self.read_exact_until(16, false, deadline)?)?;
            check_pass(cancel, deadline)?;
            if status.fatal || status.transparency_error || status.lid_open {
                return Err(Error::Protocol(format!(
                    "Scanner status after rejected start: {status:?}"
                )));
            }
            let warming = status.warming_up;
            if observations.is_empty() && !warming {
                return Ok(None);
            }
            if status.busy && !warming {
                return Err(Error::Busy(
                    "Scanner remains busy after start preparation".into(),
                ));
            }
            if observations.is_empty() {
                log::info!(
                    "Scanner rejected the first start and reports warming up; waiting for preparation"
                );
            }
            observations.push(StartStatusObservation {
                elapsed_seconds: started.elapsed().as_secs_f64(),
                status,
            });
            if !warming {
                return Ok(Some(StartRecovery {
                    rejected_header_hex: hex(rejected),
                    status_observations: observations,
                    warmup_seconds: started.elapsed().as_secs_f64(),
                }));
            }
            std::thread::sleep(
                Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
}

fn check_pass(cancel: &AtomicBool, deadline: Instant) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(Error::Timeout("Pass deadline".into()));
    }
    Ok(())
}

fn resumed_start_error(error: Error) -> Error {
    let context = |message: String| {
        format!("Resumed scan start failed after two start attempts; no further retry: {message}")
    };
    match error {
        Error::Protocol(message) => Error::Protocol(context(message)),
        Error::Busy(message) => Error::Busy(context(message)),
        Error::Timeout(message) => Error::Timeout(context(message)),
        Error::NotFound(message) => Error::NotFound(context(message)),
        Error::Driver(message) => Error::Driver(context(message)),
        Error::Unsupported { feature, reason } => Error::Unsupported {
            feature,
            reason: context(reason),
        },
        other => other,
    }
}

fn deadline(timeout: Duration, phase: &str) -> Result<Instant> {
    if timeout.is_zero() {
        return Err(Error::Invalid(format!("{phase} timeout must be positive")));
    }
    Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| Error::Invalid(format!("{phase} timeout is too large")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    struct Fragmented {
        input: VecDeque<u8>,
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        timeouts: Arc<Mutex<Vec<Duration>>>,
    }
    impl Transport for Fragmented {
        fn read(&mut self, size: usize, timeout: Duration) -> Result<Vec<u8>> {
            self.timeouts.lock().unwrap().push(timeout);
            Ok(self
                .input
                .drain(..size.min(1).min(self.input.len()))
                .collect())
        }
        fn write(&mut self, bytes: &[u8], timeout: Duration) -> Result<usize> {
            self.writes.lock().unwrap().push(bytes.to_vec());
            self.timeouts.lock().unwrap().push(timeout);
            Ok(bytes.len())
        }
    }

    #[test]
    fn fragmented_parameter_data_can_start_with_nak_byte() {
        let transport = Fragmented {
            input: [0x15, 0, 0, 0].into(),
            writes: Arc::default(),
            timeouts: Arc::default(),
        };
        let mut protocol = Esci::new(Box::new(transport), Duration::from_secs(1)).unwrap();
        assert_eq!(protocol.query(b"\x1cS", 4).unwrap(), [0x15, 0, 0, 0]);
    }

    struct TimedOutFragments {
        chunks: VecDeque<Vec<u8>>,
        reads: Arc<Mutex<Vec<usize>>>,
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl Transport for TimedOutFragments {
        fn read(&mut self, size: usize, _: Duration) -> Result<Vec<u8>> {
            self.reads.lock().unwrap().push(size);
            let chunk = self.chunks.pop_front().ok_or_else(|| {
                Error::Timeout("reading scanner: simulated transport timeout".into())
            })?;
            assert!(chunk.len() <= size);
            Ok(chunk)
        }
        fn write(&mut self, bytes: &[u8], _: Duration) -> Result<usize> {
            self.writes.lock().unwrap().push(bytes.to_vec());
            Ok(bytes.len())
        }
    }

    #[test]
    fn split_incomplete_block_timeout_reports_counts_without_ack_or_partial_payload() {
        let mut header = vec![2, 0x12];
        for field in [8u32, 3, 4] {
            header.extend(field.to_le_bytes());
        }
        let reads = Arc::default();
        let writes = Arc::default();
        let transport = TimedOutFragments {
            // One full data+status block, then data+status minus three bytes,
            // a one-byte fragment, and timeout with exactly two still missing.
            chunks: [header, b"abcdefgh\0".to_vec(), b"ijklmn".to_vec(), vec![0]].into(),
            reads: Arc::clone(&reads),
            writes: Arc::clone(&writes),
        };
        let mut protocol = Esci::new(Box::new(transport), Duration::from_secs(1)).unwrap();
        let mut sink = Vec::new();
        let mut progress = Vec::new();
        let error = protocol
            .acquire(
                28,
                &mut sink,
                &AtomicBool::new(false),
                Duration::from_secs(2),
                &mut |done, total| {
                    progress.push((done, total));
                    true
                },
            )
            .unwrap_err();
        let Error::Timeout(message) = error else {
            panic!("Expected timeout, got {error}");
        };
        assert_eq!(
            message,
            "Image block 2/4 (completed payload 8/28 bytes; expecting 9 data+status bytes): Read 7/9 bytes: reading scanner: simulated transport timeout"
        );
        assert_eq!(sink, b"abcdefgh");
        assert_eq!(progress, [(8, 28)]);
        assert_eq!(*reads.lock().unwrap(), [14, 9, 9, 3, 2]);
        assert_eq!(*writes.lock().unwrap(), [b"\x1cG".to_vec(), vec![6]]);
    }

    #[test]
    fn transport_timeout_after_lone_nak_remains_unsupported() {
        let transport = TimedOutFragments {
            chunks: [vec![0x15]].into(),
            reads: Arc::default(),
            writes: Arc::default(),
        };
        let mut protocol = Esci::new(Box::new(transport), Duration::from_secs(1)).unwrap();
        assert!(matches!(protocol.query(b"\x1cS", 64),
            Err(Error::Unsupported { reason, .. }) if reason == "device NAK"));
    }

    #[test]
    fn pass_deadline_bounds_each_io_operation() {
        let mut input = vec![2, 0x12];
        for field in [3u32, 1, 0] {
            input.extend(field.to_le_bytes());
        }
        input.extend(b"abc\0");
        let timeouts = Arc::default();
        let transport = Fragmented {
            input: input.into(),
            writes: Arc::default(),
            timeouts: Arc::clone(&timeouts),
        };
        let mut protocol = Esci::new(Box::new(transport), Duration::from_secs(10)).unwrap();
        let mut image = Vec::new();
        protocol
            .acquire(
                3,
                &mut image,
                &AtomicBool::new(false),
                Duration::from_secs(1),
                &mut |_, _| true,
            )
            .unwrap();
        assert_eq!(image, b"abc");
        assert!(
            timeouts
                .lock()
                .unwrap()
                .iter()
                .all(|timeout| *timeout <= Duration::from_secs(1))
        );
    }

    #[test]
    fn invalid_pass_timeout_sends_no_commands() {
        let writes = Arc::default();
        let transport = Fragmented {
            input: VecDeque::new(),
            writes: Arc::clone(&writes),
            timeouts: Arc::default(),
        };
        let mut protocol = Esci::new(Box::new(transport), Duration::from_secs(1)).unwrap();
        assert!(matches!(
            protocol.acquire(
                3,
                &mut Vec::new(),
                &AtomicBool::new(false),
                Duration::ZERO,
                &mut |_, _| true
            ),
            Err(Error::Invalid(_))
        ));
        assert!(writes.lock().unwrap().is_empty());
    }

    #[test]
    fn device_fault_ends_transfer_without_ack_or_cancel_or_successful_progress() {
        for status in [0x40, 0x80, 0xc0, 0x50, 0x90, 0xd0] {
            for blocks in [1u32, 2] {
                let mut input = vec![2, 0x12];
                for field in [3, blocks, 0] {
                    input.extend(field.to_le_bytes());
                }
                input.extend([b'a', b'b', b'c', status]);
                let writes = Arc::default();
                let transport = Fragmented {
                    input: input.into(),
                    writes: Arc::clone(&writes),
                    timeouts: Arc::default(),
                };
                let mut protocol = Esci::new(Box::new(transport), Duration::from_secs(1)).unwrap();
                let mut sink = Vec::new();
                let error = protocol
                    .acquire(
                        u64::from(3 * blocks),
                        &mut sink,
                        &AtomicBool::new(false),
                        Duration::from_secs(1),
                        &mut |_, _| panic!("Faulted block must not advance progress"),
                    )
                    .unwrap_err();
                let Error::Protocol(message) = error else {
                    panic!("Expected block status error, got {error}");
                };
                assert!(message.contains(&format!("Image block 1/{blocks}")));
                assert!(message.contains(&format!("received payload 3/{} bytes", 3 * blocks)));
                assert!(message.contains(&format!("status {status:02x}")));
                assert!(message.contains("scanner ended the image transfer"));
                assert_eq!(sink, b"abc");
                assert_eq!(*writes.lock().unwrap(), [b"\x1cG".to_vec()]);
            }
        }
    }

    #[test]
    fn failed_cancel_retains_the_request_source_and_transfer_position() {
        for (status, flag, continuing, reason) in [
            (0, false, false, "progress callback requested cancellation"),
            (0, true, true, "scan cancellation was requested"),
            (0x10, false, true, "scanner requested cancellation"),
        ] {
            let mut input = vec![2, 0x12];
            for field in [3u32, 2, 0] {
                input.extend(field.to_le_bytes());
            }
            input.extend([b'a', b'b', b'c', status, 2]);
            let writes = Arc::default();
            let transport = Fragmented {
                input: input.into(),
                writes: Arc::clone(&writes),
                timeouts: Arc::default(),
            };
            let mut protocol = Esci::new(Box::new(transport), Duration::from_secs(1)).unwrap();
            let cancel = AtomicBool::new(false);
            let error = protocol
                .acquire(
                    6,
                    &mut Vec::new(),
                    &cancel,
                    Duration::from_secs(1),
                    &mut |_, _| {
                        cancel.store(flag, Ordering::Relaxed);
                        continuing
                    },
                )
                .unwrap_err();
            let Error::Protocol(message) = error else {
                panic!("Expected cancellation handshake error, got {error}");
            };
            assert!(message.contains(reason));
            assert!(message.contains("Image block 1/2 (received payload 3/6 bytes"));
            assert!(message.contains("cancellation handshake also failed"));
            assert!(message.contains("got 02"));
            assert_eq!(*writes.lock().unwrap(), [b"\x1cG".to_vec(), vec![0x18]]);
        }
    }

    #[test]
    fn failed_cancel_does_not_hide_pass_deadline() {
        let mut input = vec![2, 0x12];
        for field in [3u32, 2, 0] {
            input.extend(field.to_le_bytes());
        }
        input.extend(b"abc\0\x02");
        let writes = Arc::default();
        let transport = Fragmented {
            input: input.into(),
            writes: Arc::clone(&writes),
            timeouts: Arc::default(),
        };
        let mut protocol = Esci::new(Box::new(transport), Duration::from_secs(1)).unwrap();
        let error = protocol
            .acquire(
                6,
                &mut Vec::new(),
                &AtomicBool::new(false),
                Duration::from_millis(100),
                &mut |_, _| {
                    std::thread::sleep(Duration::from_millis(110));
                    true
                },
            )
            .unwrap_err();
        let Error::Timeout(message) = error else {
            panic!("Expected pass deadline timeout, got {error}");
        };
        assert!(message.contains("Image block 1/2 (received payload 3/6 bytes"));
        assert!(message.contains("pass deadline expired"));
        assert!(message.contains("cancellation handshake also failed"));
        assert_eq!(*writes.lock().unwrap(), [b"\x1cG".to_vec(), vec![0x18]]);
    }

    #[test]
    fn healthy_transfer_cancellation_only_sends_can_at_nonfinal_boundary() {
        for blocks in [1u32, 2] {
            let mut input = vec![2, 0x12];
            for field in [3, blocks, 0] {
                input.extend(field.to_le_bytes());
            }
            input.extend(b"abc\0\x06");
            let writes = Arc::default();
            let transport = Fragmented {
                input: input.into(),
                writes: Arc::clone(&writes),
                timeouts: Arc::default(),
            };
            let mut protocol = Esci::new(Box::new(transport), Duration::from_secs(1)).unwrap();
            assert!(matches!(
                protocol.acquire(
                    u64::from(3 * blocks),
                    &mut Vec::new(),
                    &AtomicBool::new(false),
                    Duration::from_secs(1),
                    &mut |_, _| false,
                ),
                Err(Error::Cancelled)
            ));
            let mut expected = vec![b"\x1cG".to_vec()];
            if blocks > 1 {
                expected.push(vec![0x18]);
            }
            assert_eq!(*writes.lock().unwrap(), expected);
        }
    }

    #[test]
    fn disk_failure_retains_priority_over_scanner_or_cancellation_errors() {
        struct FullDisk;
        impl Write for FullDisk {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("disk full"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for status in [0, 0x40, 0x80] {
            let mut input = vec![2, 0x12];
            for field in [3u32, 2, 0] {
                input.extend(field.to_le_bytes());
            }
            input.extend([b'a', b'b', b'c', status, 0x15]);
            let writes = Arc::default();
            let transport = Fragmented {
                input: input.into(),
                writes: Arc::clone(&writes),
                timeouts: Arc::default(),
            };
            let mut protocol = Esci::new(Box::new(transport), Duration::from_secs(1)).unwrap();
            let error = protocol
                .acquire(
                    6,
                    &mut FullDisk,
                    &AtomicBool::new(false),
                    Duration::from_secs(1),
                    &mut |_, _| panic!("Disk failure must not advance progress"),
                )
                .unwrap_err();
            let Error::Io(error) = error else {
                panic!("Expected disk error, got {error}");
            };
            let message = error.to_string();
            assert!(message.contains("disk full"));
            let mut expected = vec![b"\x1cG".to_vec()];
            if status == 0 {
                assert!(message.contains("cancellation handshake also failed"));
                expected.push(vec![0x18]);
            } else {
                assert!(message.contains("scanner also ended the transfer"));
                assert!(message.contains(&format!("status {status:02x}")));
            }
            assert_eq!(*writes.lock().unwrap(), expected);
        }
    }

    #[test]
    fn parameter_readback_checks_main_lamp_mode() {
        let settings = ScanSettings {
            gamma: Gamma::DeviceDefault,
            ..Default::default()
        };
        let mut actual = settings.parameters(false).unwrap();
        actual[38] = 1;
        let mut input = vec![6; 5];
        input.extend(actual);
        let transport = Fragmented {
            input: input.into(),
            writes: Arc::default(),
            timeouts: Arc::default(),
        };
        let mut protocol = Esci::new(Box::new(transport), Duration::from_secs(1)).unwrap();
        assert!(
            matches!(protocol.configure(&settings, false), Err(Error::Protocol(message)) if message.contains("Parameter readback mismatch"))
        );
    }

    fn rejected_start() -> Vec<u8> {
        vec![2, 0x92, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    }

    fn scanner_status(flags: u8, source_flags: u8) -> Vec<u8> {
        let mut response = vec![0; 16];
        response[0] = flags;
        response[2] = source_flags;
        response[9] = 0x80;
        response
    }

    fn successful_image() -> Vec<u8> {
        let mut response = vec![2, 0x12];
        for field in [3u32, 1, 0] {
            response.extend(field.to_le_bytes());
        }
        response.extend(b"abc\0");
        response
    }

    type RecordedWrites = Arc<Mutex<Vec<Vec<u8>>>>;

    fn recovering_protocol(input: Vec<u8>) -> (Esci, RecordedWrites) {
        let writes = Arc::default();
        let transport = Fragmented {
            input: input.into(),
            writes: Arc::clone(&writes),
            timeouts: Arc::default(),
        };
        (
            Esci::new(Box::new(transport), Duration::from_secs(1)).unwrap(),
            writes,
        )
    }

    fn acquire_with_v800_policy(
        protocol: &mut Esci,
        cancel: &AtomicBool,
    ) -> Result<TransferResult> {
        protocol.acquire_with_policy(
            3,
            &mut Vec::new(),
            cancel,
            Duration::from_secs(3),
            &mut |_, _| true,
            &crate::capabilities::V800_FAMILY.transfer,
        )
    }

    #[test]
    fn confirmed_start_warmup_resumes_once_without_reconfiguration() {
        let mut input = rejected_start();
        input.extend(scanner_status(3, 0xc0));
        input.extend(scanner_status(1, 0xc0));
        input.extend(successful_image());
        let (mut protocol, writes) = recovering_protocol(input);
        let mut image = Vec::new();
        let result = protocol
            .acquire_with_policy(
                3,
                &mut image,
                &AtomicBool::new(false),
                Duration::from_secs(3),
                &mut |_, _| true,
                &crate::capabilities::V800_FAMILY.transfer,
            )
            .unwrap();
        assert_eq!(image, b"abc");
        assert_eq!(result.start_attempts, 2);
        let recovery = result.start_recovery.unwrap();
        assert_eq!(recovery.rejected_header_hex, hex(&rejected_start()));
        assert_eq!(recovery.status_observations.len(), 2);
        assert!(recovery.status_observations[0].status.warming_up);
        assert!(!recovery.status_observations[1].status.warming_up);
        assert!(recovery.warmup_seconds >= recovery.status_observations[1].elapsed_seconds);
        assert_eq!(
            *writes.lock().unwrap(),
            [b"\x1cG", b"\x1cF", b"\x1cF", b"\x1cG"]
        );
    }

    #[test]
    fn strict_acquire_and_disabled_model_policy_never_probe_rejection() {
        let mut policy = crate::capabilities::V800_FAMILY.transfer;
        policy.allow_start_warmup_recovery = false;
        for use_policy in [false, true] {
            let (mut protocol, writes) = recovering_protocol(rejected_start());
            let error = if use_policy {
                protocol.acquire_with_policy(
                    3,
                    &mut Vec::new(),
                    &AtomicBool::new(false),
                    Duration::from_secs(1),
                    &mut |_, _| true,
                    &policy,
                )
            } else {
                protocol.acquire(
                    3,
                    &mut Vec::new(),
                    &AtomicBool::new(false),
                    Duration::from_secs(1),
                    &mut |_, _| true,
                )
            }
            .unwrap_err();
            assert!(matches!(error, Error::Protocol(_)));
            assert_eq!(*writes.lock().unwrap(), [b"\x1cG"]);
        }
    }

    #[test]
    fn rejected_start_requires_warming_without_fatal_or_source_error() {
        for status in [
            scanner_status(1, 0xc0),
            scanner_status(0x83, 0xc0),
            scanner_status(3, 0xe0),
            scanner_status(3, 0xc2),
        ] {
            let mut input = rejected_start();
            input.extend(status);
            input.extend(successful_image());
            let (mut protocol, writes) = recovering_protocol(input);
            assert!(matches!(
                acquire_with_v800_policy(&mut protocol, &AtomicBool::new(false)),
                Err(Error::Protocol(_))
            ));
            assert_eq!(*writes.lock().unwrap(), [b"\x1cG", b"\x1cF"]);
        }
    }

    #[test]
    fn second_start_rejection_is_terminal_without_another_status_query() {
        let mut input = rejected_start();
        input.extend(scanner_status(3, 0xc0));
        input.extend(scanner_status(1, 0xc0));
        input.extend(rejected_start());
        let (mut protocol, writes) = recovering_protocol(input);
        assert!(matches!(
            acquire_with_v800_policy(&mut protocol, &AtomicBool::new(false)),
            Err(Error::Protocol(message)) if message.contains("after two start attempts; no further retry")
        ));
        assert_eq!(
            *writes.lock().unwrap(),
            [b"\x1cG", b"\x1cF", b"\x1cF", b"\x1cG"]
        );
    }

    #[test]
    fn unknown_start_framing_or_geometry_never_triggers_status_or_retry() {
        let mut malformed = rejected_start();
        malformed[0] = 3;
        let mut nonzero_length = rejected_start();
        nonzero_length[2] = 1;
        let mut different_fatal = rejected_start();
        different_fatal[1] = 0x82;
        for input in [
            malformed,
            nonzero_length,
            different_fatal,
            rejected_start()[..13].to_vec(),
            vec![0x15],
        ] {
            let (mut protocol, writes) = recovering_protocol(input);
            assert!(acquire_with_v800_policy(&mut protocol, &AtomicBool::new(false)).is_err());
            assert_eq!(*writes.lock().unwrap(), [b"\x1cG"]);
        }
    }

    #[test]
    fn lost_status_reply_after_rejection_never_restarts() {
        let mut input = rejected_start();
        input.extend(scanner_status(3, 0xc0)[..15].iter());
        let (mut protocol, writes) = recovering_protocol(input);
        assert!(acquire_with_v800_policy(&mut protocol, &AtomicBool::new(false)).is_err());
        assert_eq!(*writes.lock().unwrap(), [b"\x1cG", b"\x1cF"]);
    }

    #[test]
    fn unhealthy_status_after_warming_never_restarts() {
        for status in [
            scanner_status(0x81, 0xc0),
            scanner_status(1, 0xe0),
            scanner_status(0x41, 0xc0),
            scanner_status(1, 0xc2),
        ] {
            let mut input = rejected_start();
            input.extend(scanner_status(3, 0xc0));
            input.extend(status);
            let (mut protocol, writes) = recovering_protocol(input);
            assert!(acquire_with_v800_policy(&mut protocol, &AtomicBool::new(false)).is_err());
            assert_eq!(*writes.lock().unwrap(), [b"\x1cG", b"\x1cF", b"\x1cF"]);
        }
    }

    #[test]
    fn pass_deadline_expires_during_start_warmup_without_second_start() {
        let mut input = rejected_start();
        input.extend(scanner_status(3, 0xc0));
        input.extend(scanner_status(1, 0xc0));
        input.extend(successful_image());
        let (mut protocol, writes) = recovering_protocol(input);
        let result = protocol.acquire_with_policy(
            3,
            &mut Vec::new(),
            &AtomicBool::new(false),
            Duration::from_millis(100),
            &mut |_, _| true,
            &crate::capabilities::V800_FAMILY.transfer,
        );
        assert!(matches!(result, Err(Error::Timeout(_))));
        assert_eq!(*writes.lock().unwrap(), [b"\x1cG", b"\x1cF"]);
    }

    #[test]
    fn cancellation_during_start_warmup_prevents_second_start() {
        struct CancelOnWarmup {
            inner: Fragmented,
            cancel: Arc<AtomicBool>,
        }
        impl Transport for CancelOnWarmup {
            fn read(&mut self, size: usize, timeout: Duration) -> Result<Vec<u8>> {
                let response = self.inner.read(size, timeout)?;
                if self.inner.input.is_empty() {
                    self.cancel.store(true, Ordering::Relaxed);
                }
                Ok(response)
            }
            fn write(&mut self, bytes: &[u8], timeout: Duration) -> Result<usize> {
                self.inner.write(bytes, timeout)
            }
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let writes = Arc::default();
        let mut input = rejected_start();
        input.extend(scanner_status(3, 0xc0));
        let transport = CancelOnWarmup {
            inner: Fragmented {
                input: input.into(),
                writes: Arc::clone(&writes),
                timeouts: Arc::default(),
            },
            cancel: Arc::clone(&cancel),
        };
        let mut protocol = Esci::new(Box::new(transport), Duration::from_secs(1)).unwrap();
        assert!(matches!(
            acquire_with_v800_policy(&mut protocol, &cancel),
            Err(Error::Cancelled)
        ));
        assert_eq!(*writes.lock().unwrap(), [b"\x1cG", b"\x1cF"]);
    }
}
