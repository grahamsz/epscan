// SPDX-License-Identifier: MIT
//! One explicit, bounded ESC/I sequence experiment. Build with `--features cli`.
//! Every invocation performs at most one acquisition, without recovery or retries.

#[cfg(not(feature = "cli"))]
fn main() {
    eprintln!("Build this hardware experiment with --features cli");
    std::process::exit(2);
}

#[cfg(feature = "cli")]
fn main() {
    if let Err(error) = experiment::main() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(feature = "cli")]
mod experiment {
    use clap::Parser;
    use epscan::{
        Backend, Error, Gamma, Result, ScanMode, ScanSettings, Source, capabilities::scanner_model,
        protocol::hex, session::esci::Esci, transport,
    };
    use serde::Serialize;
    use serde_json::{Value, json};
    use std::{
        fs::{self, File, OpenOptions},
        io::{Seek, Write},
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    const IO_TIMEOUT: Duration = Duration::from_secs(15);
    const PASS_TIMEOUT: Duration = Duration::from_secs(90);
    const MAX_BYTES: u64 = 16 * 1024 * 1024;

    #[derive(Debug, Parser, Serialize)]
    #[command(about = "Perform one bounded primary-TPU acquisition experiment; no retries")]
    struct Args {
        /// New output prefix: .json, .protocol.jsonl, .partial.bin / .bin.
        #[arg(long)]
        name: PathBuf,
        /// Exact device location; required when more than one scanner is present.
        #[arg(long)]
        device: Option<String>,
        #[arg(long, value_enum, default_value = "auto")]
        backend: Backend,
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u32).range(25..=12800))]
        dpi: u32,
        #[arg(long, default_value_t = 16, value_parser = parse_depth)]
        depth: u8,
        /// Rectangle width from x=0, positive and at most 100 mm.
        #[arg(long, default_value_t = 10.0, value_parser = parse_extent)]
        width_mm: f64,
        /// Rectangle height from y=0, positive and at most 100 mm.
        #[arg(long, default_value_t = 10.0, value_parser = parse_extent)]
        height_mm: f64,
        /// Send the documented ESC e source-enable request with byte 1.
        #[arg(long)]
        source_enable: bool,
        #[arg(long)]
        skip_reset: bool,
        #[arg(long)]
        skip_focus: bool,
        #[arg(long)]
        lut_before_parameters: bool,
        /// Delay after complete setup/readback, before readiness and acquisition.
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u64).range(0..=2000))]
        settle_ms: u64,
        #[arg(long, default_value_t = 32, value_parser = clap::value_parser!(u8).range(1..))]
        lines: u8,
        #[arg(long, value_enum, default_value = "device-default")]
        gamma: Gamma,
        /// Set preview byte 27 without changing the requested DPI or depth.
        #[arg(long)]
        preview: bool,
        /// Experiment with zero halftone/threshold bytes 32/33 from VueScan.
        #[arg(long)]
        neutral_processing: bool,
        /// Experimental monochrome mode 00 from the reference VueScan trace.
        #[arg(long)]
        mono: bool,
        /// Observe status after a fully parsed FS G 0x92 rejection; never retry.
        #[arg(long)]
        observe_rejection: bool,
    }

    fn parse_depth(text: &str) -> std::result::Result<u8, String> {
        match text {
            "8" => Ok(8),
            "16" => Ok(16),
            _ => Err("depth must be 8 or 16".into()),
        }
    }

    fn parse_extent(text: &str) -> std::result::Result<f64, String> {
        let extent: f64 = text
            .parse()
            .map_err(|_| "extent must be a number".to_owned())?;
        if extent.is_finite() && extent > 0.0 && extent <= 100.0 {
            Ok(extent)
        } else {
            Err("extent must be finite, positive and at most 100 mm".into())
        }
    }

    fn now() -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
    }

    fn suffix(prefix: &Path, tail: &str) -> PathBuf {
        let mut path = prefix.as_os_str().to_os_string();
        path.push(tail);
        path.into()
    }

    fn create_new(path: &Path) -> Result<File> {
        Ok(OpenOptions::new().write(true).create_new(true).open(path)?)
    }

    fn checkpoint(file: &mut File, metadata: &Value) -> Result<()> {
        file.rewind()?;
        file.set_len(0)?;
        serde_json::to_writer_pretty(&mut *file, metadata)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        Ok(())
    }

    fn stage(metadata: &mut Value, name: &str) {
        metadata["stage"] = name.into();
        metadata["updated_unix"] = now().into();
    }

    fn check_cancel(cancel: &AtomicBool) -> Result<()> {
        if cancel.load(Ordering::Relaxed) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }

    fn is_observable_rejection(error: &Error) -> bool {
        matches!(error, Error::Protocol(message)
            if message.starts_with("Scanner rejected scan start (FS G status 0x92,"))
    }

    fn verify_rejection_trace(path: &Path) -> std::result::Result<(), String> {
        let trace = fs::read_to_string(path)
            .map_err(|error| format!("Cannot read rejection trace: {error}"))?;
        let last = trace
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .ok_or_else(|| "Rejection trace is empty".to_owned())?;
        let event: Value = serde_json::from_str(last)
            .map_err(|error| format!("Cannot parse final rejection trace entry: {error}"))?;
        if event["direction"].as_str() == Some("in")
            && event["length"].as_u64() == Some(14)
            && event["image"].as_bool() == Some(false)
            && event["hex"].as_str() == Some("0292000000000000000000000000")
        {
            Ok(())
        } else {
            Err("Final trace entry is not an exact 14-byte FS G 0x92 header with zero transfer lengths".into())
        }
    }

    /// Only the complete, parsed start-rejection header establishes a known
    /// command boundary. Do not probe after timeout, short header, unexpected
    /// ACK, image-stream failure, or any other error with unknown framing.
    fn observe_rejection(protocol: &mut Esci, cancel: &AtomicBool) -> Value {
        const LIMIT: Duration = Duration::from_secs(30);
        const STATUS_IO: Duration = Duration::from_secs(1);
        let started = Instant::now();
        let previous_timeout = protocol.io_timeout;
        protocol.io_timeout = STATUS_IO;
        let mut observations = Vec::new();
        let stop_reason = loop {
            if cancel.load(Ordering::Relaxed) {
                break "cancelled";
            }
            // A status transaction performs one write and one read. Reserve
            // both whole-second usbscan timeouts within the 30-second bound.
            if LIMIT.saturating_sub(started.elapsed()) < STATUS_IO * 2 {
                break "observation_time_limit";
            }
            let query_started = started.elapsed().as_secs_f64();
            match protocol.status() {
                Ok(status) => {
                    let warming_up = status.warming_up;
                    observations.push(json!({
                        "query_started_seconds":query_started,
                        "query_completed_seconds":started.elapsed().as_secs_f64(),
                        "status":status
                    }));
                    if !warming_up {
                        break "not_warming_up";
                    }
                }
                Err(error) => {
                    observations.push(json!({
                        "query_started_seconds":query_started,
                        "query_completed_seconds":started.elapsed().as_secs_f64(),
                        "query_error":error.to_string()
                    }));
                    // A failed status query invalidates the known boundary.
                    break "status_query_failed";
                }
            }
            std::thread::sleep(Duration::from_millis(250));
        };
        protocol.io_timeout = previous_timeout;
        json!({
            "policy":"FS F only after complete FS G 0x92 header; no reset or retry",
            "limit_seconds":30,"poll_interval_ms":250,"status_io_timeout_seconds":1,
            "elapsed_seconds":started.elapsed().as_secs_f64(),
            "stop_reason":stop_reason,"observations":observations
        })
    }

    struct Configuration<'a> {
        protocol: &'a mut Esci,
        metadata: &'a mut Value,
        cancel: &'a AtomicBool,
    }

    impl Configuration<'_> {
        fn transaction(&mut self, name: &str, command: &[u8], body: Option<&[u8]>) -> Result<()> {
            check_cancel(self.cancel)?;
            stage(self.metadata, name);
            self.protocol.simple(command)?;
            if let Some(body) = body {
                self.protocol.simple(body)?;
            }
            Ok(())
        }

        fn identity_luts(&mut self) -> Result<()> {
            for channel in *b"RGB" {
                let table: Vec<u8> = std::iter::once(channel).chain(0u8..=255).collect();
                self.transaction(
                    &format!("identity-lut-{}", char::from(channel)),
                    b"\x1bz",
                    Some(&table),
                )?;
            }
            Ok(())
        }
    }

    pub fn main() -> Result<()> {
        let args = Args::parse();
        if args.name.file_name().is_none() {
            return Err(Error::Invalid(
                "--name must have a file-name component".into(),
            ));
        }
        if args.lut_before_parameters && args.gamma != Gamma::IdentityLut {
            return Err(Error::Invalid(
                "--lut-before-parameters requires --gamma identity-lut".into(),
            ));
        }
        let settings = ScanSettings {
            mode: if args.mono {
                ScanMode::Gray
            } else {
                ScanMode::Rgb
            },
            rect_mm: [0., 0., args.width_mm, args.height_mm],
            dpi: args.dpi,
            y_oversampling: 1,
            depth: args.depth,
            source: Source::Transparency,
            gamma: args.gamma,
            preview: args.preview,
        };
        // Bound the experiment before enumeration/opening, then validate again
        // against the connected scanner's actual identity and profile.
        let channels = if args.mono { 1u8 } else { 3u8 };
        let pixels = settings.pixels()?;
        let expected = u64::from(pixels[2])
            * u64::from(pixels[3])
            * u64::from(channels)
            * u64::from(settings.depth / 8);
        if expected == 0 || expected > MAX_BYTES {
            return Err(Error::Invalid(format!(
                "Experiment size {expected} exceeds the 16 MiB limit"
            )));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancel);
        ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed)).map_err(|error| {
            Error::Invalid(format!("Cannot install cancellation handler: {error}"))
        })?;

        if let Some(parent) = args
            .name
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let manifest_path = suffix(&args.name, ".json");
        let trace_path = suffix(&args.name, ".protocol.jsonl");
        let partial_path = suffix(&args.name, ".partial.bin");
        let payload_path = suffix(&args.name, ".bin");
        // The new manifest reserves the prefix for this invocation. Check every
        // output first so an earlier attempt is never silently replaced.
        for path in [&manifest_path, &trace_path, &partial_path, &payload_path] {
            if path.try_exists()? {
                return Err(Error::Invalid(format!(
                    "Output already exists: {}",
                    path.display()
                )));
            }
        }
        let mut manifest = create_new(&manifest_path)?;
        let mut metadata = json!({
            "schema":1, "experiment":"probe_sequence", "version":env!("CARGO_PKG_VERSION"),
            "complete":false, "started_unix":now(), "arguments":args, "settings":settings,
            "pixels":pixels, "channels":channels, "expected_bytes":expected,
            "byte_order":"little-endian", "experimental_monochrome":args.mono,
            "source":"primary-transparency", "rect_mm":settings.rect_mm,
            "io_timeout_seconds":15, "pass_timeout_seconds":90,
            "acquisition_attempts":0, "automatic_retries":0,
            "trace_file":trace_path, "partial_payload_file":partial_path,
            "settle_policy":"after complete setup/readback before readiness and acquisition",
            "initial_diagnostics_policy":"FS F and FS S before optional reset",
            "stage":"reserved"
        });
        checkpoint(&mut manifest, &metadata)?;

        let result: Result<()> = (|| {
            let trace = create_new(&trace_path)?;
            let mut payload = create_new(&partial_path)?;
            stage(&mut metadata, "discover");
            check_cancel(&cancel)?;
            let mut devices = epscan::list_devices(args.backend)?;
            if let Some(location) = &args.device {
                devices.retain(|device| &device.location == location);
            }
            if devices.len() != 1 {
                return Err(Error::Invalid(format!(
                    "Need exactly one supported device, found {}; use --device",
                    devices.len()
                )));
            }
            let device = devices.remove(0);
            let profile = scanner_model(device.vid, device.pid)
                .ok_or_else(|| Error::Invalid("Device has no registered profile".into()))?;
            metadata["device"] = serde_json::to_value(&device)?;
            stage(&mut metadata, "open");
            let mut protocol = Esci::new(transport::open(&device)?, IO_TIMEOUT)?;
            protocol.trace = Some(trace);
            stage(&mut metadata, "identify");
            let (legacy, extended, capabilities) = protocol.identify()?;
            metadata["legacy_identity_hex"] = hex(&legacy).into();
            metadata["extended_identity_hex"] = hex(&extended).into();
            metadata["capabilities"] = serde_json::to_value(&capabilities)?;
            profile.validate_identity(&capabilities)?;
            let mut parameters = settings.parameters_with_capabilities(&capabilities, false)?;
            if settings.pixels_for(profile)? != pixels {
                return Err(Error::Invalid(
                    "Connected model geometry differs from this experiment".into(),
                ));
            }
            parameters[28] = args.lines;
            if args.neutral_processing {
                parameters[32] = 0;
                parameters[33] = 0;
            }
            metadata["parameters_requested_hex"] = hex(&parameters).into();
            stage(&mut metadata, "initial-diagnostics");
            metadata["initial_status"] = serde_json::to_value(protocol.status()?)?;
            metadata["initial_parameters_hex"] = hex(&protocol.query(b"\x1cS", 64)?).into();

            let mut configure = Configuration {
                protocol: &mut protocol,
                metadata: &mut metadata,
                cancel: &cancel,
            };
            if !args.skip_reset {
                configure.transaction("reset", b"\x1b@", None)?;
            }
            if args.source_enable {
                configure.transaction("source-enable", b"\x1be", Some(&[1]))?;
            }
            if args.lut_before_parameters {
                configure.identity_luts()?;
            }
            configure.transaction("parameters", b"\x1cW", Some(&parameters))?;
            if !args.skip_focus {
                configure.transaction(
                    "focus",
                    b"\x1bp",
                    Some(&[capabilities.optics(settings.source)?.focus_command_position]),
                )?;
            }
            if args.gamma == Gamma::IdentityLut && !args.lut_before_parameters {
                configure.identity_luts()?;
            }
            stage(&mut metadata, "readback");
            let actual = protocol.query(b"\x1cS", 64)?;
            metadata["parameters_readback_hex"] = hex(&actual).into();
            if actual[..39] != parameters[..39] {
                return Err(Error::Protocol(
                    "Parameter readback mismatch through byte 38".into(),
                ));
            }
            stage(&mut metadata, "settle");
            let settling = Instant::now();
            let delay = Duration::from_millis(args.settle_ms);
            while settling.elapsed() < delay {
                check_cancel(&cancel)?;
                std::thread::sleep(
                    Duration::from_millis(20).min(delay.saturating_sub(settling.elapsed())),
                );
            }
            stage(&mut metadata, "ready");
            protocol.wait_ready(&cancel, PASS_TIMEOUT)?;
            metadata["readiness_completed"] = true.into();
            check_cancel(&cancel)?;
            metadata["acquisition_attempts"] = 1.into();
            stage(&mut metadata, "acquire");
            let started = Instant::now();
            let mut received = 0;
            let acquisition = protocol.acquire(
                expected,
                &mut payload,
                &cancel,
                PASS_TIMEOUT,
                &mut |done, total| {
                    received = done;
                    eprintln!("received {done}/{total} bytes");
                    true
                },
            );
            metadata["acquisition_seconds"] = started.elapsed().as_secs_f64().into();
            let transfer = match acquisition {
                Ok(transfer) => transfer,
                Err(error) => {
                    if args.observe_rejection && received == 0 && is_observable_rejection(&error) {
                        match verify_rejection_trace(&trace_path) {
                            Ok(()) => {
                                stage(&mut metadata, "observe-start-rejection");
                                metadata["rejection_observation"] =
                                    observe_rejection(&mut protocol, &cancel);
                            }
                            Err(reason) => {
                                metadata["rejection_observation_skipped"] = reason.into();
                            }
                        }
                    }
                    return Err(error);
                }
            };
            metadata["transfer"] = serde_json::to_value(transfer)?;
            // Closing the connection is the only cleanup, including errors.
            // The explicit observation flag only queries status at a validated
            // rejected-start boundary. No reset or another start follows errors.
            drop(protocol);
            payload.sync_all()?;
            drop(payload);
            // A hard link publishes the completed bytes without replacing a
            // file created by another process; removing the temporary name then
            // gives the same final .partial.bin -> .bin transition as a rename.
            fs::hard_link(&partial_path, &payload_path)?;
            fs::remove_file(&partial_path)?;
            metadata["payload_file"] = serde_json::to_value(&payload_path)?;
            metadata["partial_payload_file"] = Value::Null;
            metadata["complete"] = true.into();
            metadata["stage"] = "complete".into();
            Ok(())
        })();
        if let Err(error) = &result {
            metadata["error"] = error.to_string().into();
        }
        metadata["finished_unix"] = now().into();
        if let Ok(info) = fs::metadata(&partial_path) {
            metadata["retained_partial_bytes"] = info.len().into();
        }
        let saved = checkpoint(&mut manifest, &metadata);
        eprintln!("metadata: {}", manifest_path.display());
        result.and(saved)
    }
}
