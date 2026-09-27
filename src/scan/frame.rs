// SPDX-License-Identifier: MIT
use super::{
    PassKind, Progress, ScanOptions, ScanResult, banding, io::*, sampling::RowAverager, sharpness,
};
use crate::{
    Error, Result, ScanSettings, Session,
    protocol::hex,
    session::{image::ImageResult, now},
};
use std::{
    fs::{self, File, OpenOptions},
    path::Path,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};

pub(crate) enum CaptureUpdate {
    Started {
        source: Box<ScanResult>,
        reader: File,
    },
    Available(u64),
}

impl Session {
    /// Acquire a validated job to numbered raw payloads and optional TIFFs.
    ///
    /// A JSON manifest and protocol trace accompany each job. Existing captures
    /// are never replaced. A failed acquisition retains its partial payload and
    /// closes the session; completed earlier passes remain available. Returning
    /// false from `progress` cancels at the next safe transfer boundary.
    pub fn scan(
        &mut self,
        settings: &ScanSettings,
        options: &ScanOptions,
        basename: &Path,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(Progress<'_>) -> bool,
    ) -> Result<ScanResult> {
        self.scan_observed(settings, options, basename, cancel, progress, None)
    }

    pub(crate) fn scan_observed(
        &mut self,
        settings: &ScanSettings,
        options: &ScanOptions,
        basename: &Path,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(Progress<'_>) -> bool,
        mut observer: Option<&mut dyn FnMut(CaptureUpdate)>,
    ) -> Result<ScanResult> {
        self.protocol()?;
        let capabilities = self.capabilities.clone();
        let plan = options.plan(settings, &capabilities)?;
        let model = capabilities.scanner_model()?;
        let holder_selection = options
            .holder_selection
            .map(|selection| selection.metadata(model))
            .transpose()?;
        let ready_timeout = Duration::from_secs(model.max_ready_wait_secs);
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if let Some(parent) = basename.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let (stem, mut manifest_file, manifest_path) = reserve(basename)?;
        let mut manifest = serde_json::json!({"schema":1,"implementation":"epscan","version":env!("CARGO_PKG_VERSION"),"complete":false,"started_unix":now(),
            "device":self.device,"capabilities":self.capabilities,"requested":settings,"film":options.film,"export_tiff":options.export_tiff,
            "infrared_requested":options.infrared||options.infrared_only,"infrared_depth_requested":options.ir_depth,
            "infrared_gamma_requested":options.ir_gamma,"settle_seconds":options.settle_time.as_secs_f64(),"plan":plan,"passes":[],"linearity":"unverified",
            "registration":"not_applied; separate passes; physical offsets unmeasured"});
        if let Some(selection) = &holder_selection {
            manifest["holder_selection"] = selection.clone();
        }
        if options.measure_sharpness {
            manifest["measure_sharpness"] = true.into();
        }
        if let Some(banding) = &options.banding {
            manifest["banding_requested"] = serde_json::to_value(banding)?;
        }
        write_manifest(&mut manifest_file, &manifest)?;
        let result = (|| {
            self.protocol()?.trace = Some(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(suffix(&stem, ".protocol.jsonl"))?,
            );
            let mut result = ScanResult {
                rgb: None,
                gray: None,
                ir: None,
                thumbnail: None,
                manifest: manifest_path.clone(),
            };
            for (index, pass) in plan.passes.iter().enumerate() {
                let name = pass.kind.name();
                let pass_settings = &pass.settings;
                let ir = pass.kind == PassKind::Ir;
                if index > 0 {
                    // Host delay is explicit policy, not a readiness guarantee.
                    // A 10-second delay did not resolve RGB->IR status 0x92.
                    let settling = std::time::Instant::now();
                    let mut last_progress = std::time::Instant::now() - Duration::from_secs(1);
                    while settling.elapsed() < options.settle_time {
                        if cancel.load(Ordering::Relaxed) {
                            return Err(Error::Cancelled);
                        }
                        if last_progress.elapsed() >= Duration::from_secs(1) {
                            if !progress(Progress {
                                phase: "settling",
                                pass: index,
                                done: 0,
                                total: 1,
                            }) {
                                return Err(Error::Cancelled);
                            }
                            last_progress = std::time::Instant::now();
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
                if cancel.load(Ordering::Relaxed)
                    || !progress(Progress {
                        phase: "setup",
                        pass: index,
                        done: 0,
                        total: 1,
                    })
                {
                    return Err(Error::Cancelled);
                }
                let [x, y, width, height] = pass.pixels;
                let channels = pass.channels;
                let stride =
                    u64::from(width) * u64::from(channels) * u64::from(pass_settings.depth / 8);
                let acquisition_bytes = pass
                    .expected_bytes
                    .checked_mul(u64::from(pass_settings.y_oversampling))
                    .ok_or_else(|| Error::Invalid("Y acquisition payload size overflow".into()))?;
                let tail = pass.kind.suffix();
                let partial = suffix(&stem, &format!("{tail}.partial.bin"));
                let payload = suffix(&stem, &format!("{tail}.bin"));
                let tiff = suffix(&stem, &format!("{tail}.tiff"));
                let mut meta = serde_json::json!({"pass":name,"complete":false,"settings":pass_settings,
                    "width":width,"height":height,"channels":channels,"depth":pass_settings.depth,"dpi":pass_settings.dpi,
                    "stride_bytes":stride,"byteorder":"little","channel_layout":if ir{"single_IR_candidate"}else if channels == 1{"gray"}else{"RGB_interleaved"},
                    "requested_rect_mm":pass_settings.rect_mm,"effective_rect_mm":([x,y,width,height].map(|v|f64::from(v)*25.4/f64::from(pass_settings.dpi))),
                    "inverted":false,
                    "color_correction":"off","calibration":"device_default_preserved","gamma_verified_linear":false,
                    "orientation":"device_order; no flip/rotation; target verification pending",
                    "optics":capabilities.optics(pass_settings.source)?,
                    "effective_optical_dpi":null,"spectral_identity_verified":false});
                if pass_settings.y_oversampling > 1 {
                    meta["sampling"] = serde_json::json!({
                        "y_oversampling":pass_settings.y_oversampling,
                        "acquisition_dpi":[pass_settings.dpi,pass_settings.acquisition_y_dpi()?],
                        "acquisition_pixels":pass_settings.acquisition_pixels_for(model)?,
                        "acquisition_bytes":acquisition_bytes,
                        "output_dpi":[pass_settings.dpi,pass_settings.dpi],
                        "method":"arithmetic mean of consecutive carriage rows; round half upward",
                        "individual_rows_retained":false
                    });
                    meta["sample_transform"] =
                        "Y row averaging before packed payload; no inversion or tone conversion"
                            .into();
                }
                if let Some(selection) = &holder_selection {
                    meta["holder_selection"] = selection.clone();
                }
                manifest["passes"]
                    .as_array_mut()
                    .unwrap()
                    .push(meta.clone());
                write_manifest(&mut manifest_file, &manifest)?;
                // Establish the output sink before any configuration command
                // can move the carriage. A late collision must not start a scan.
                let mut sink = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&partial)?;
                // This independent cursor survives publication of the partial file.
                let reader = observer
                    .as_ref()
                    .map(|_| File::open(&partial))
                    .transpose()?;
                meta["partial_payload_file"] = partial.to_string_lossy().to_string().into();
                manifest["passes"][index] = meta.clone();
                write_manifest(&mut manifest_file, &manifest)?;
                let (sent, actual) = self.protocol()?.configure_with_capabilities(
                    pass_settings,
                    ir,
                    &capabilities,
                )?;
                meta["parameters_sent_hex"] = hex(&sent).into();
                meta["parameters_readback_hex"] = hex(&actual).into();
                manifest["passes"][index] = meta.clone();
                write_manifest(&mut manifest_file, &manifest)?;
                self.protocol()?
                    .wait_ready(cancel, options.pass_timeout.min(ready_timeout))?;
                if let (Some(observer), Some(reader)) = (observer.as_deref_mut(), reader) {
                    let image = ImageResult {
                        payload: payload.clone(),
                        tiff: None,
                        width,
                        height,
                        channels,
                        depth: pass_settings.depth,
                        dpi: pass_settings.dpi,
                        metadata: meta.clone(),
                    };
                    let mut source = ScanResult {
                        rgb: None,
                        gray: None,
                        ir: None,
                        thumbnail: None,
                        manifest: manifest_path.clone(),
                    };
                    match pass.kind {
                        PassKind::Rgb => source.rgb = Some(image),
                        PassKind::Gray => source.gray = Some(image),
                        PassKind::Ir => source.ir = Some(image),
                        PassKind::Thumbnail => source.thumbnail = Some(image),
                    }
                    observer(CaptureUpdate::Started {
                        source: Box::new(source),
                        reader,
                    });
                }
                let started = now();
                let committed = AtomicU64::new(0);
                let mut averaged = RowAverager::new(
                    &mut sink,
                    usize::try_from(stride)
                        .map_err(|_| Error::Invalid("Row stride exceeds address space".into()))?,
                    pass_settings.depth,
                    pass_settings.y_oversampling,
                    &committed,
                );
                let transfer = self.protocol()?.acquire_with_policy(
                    acquisition_bytes,
                    &mut averaged,
                    cancel,
                    options.pass_timeout,
                    &mut |done, total| {
                        if let Some(observer) = observer.as_deref_mut() {
                            observer(CaptureUpdate::Available(committed.load(Ordering::Acquire)));
                        }
                        progress(Progress {
                            phase: name,
                            pass: index,
                            done,
                            total,
                        })
                    },
                    &model.transfer,
                )?;
                averaged.finish()?;
                drop(averaged);
                sink.sync_all()?;
                drop(sink);
                meta["transfer"] = serde_json::to_value(transfer)?;
                meta["elapsed_seconds"] = (now() - started).into();
                meta["acquisition_complete"] = true.into();
                meta["sha256"] = sha256(&partial)?.into();
                manifest["passes"][index] = meta.clone();
                write_manifest(&mut manifest_file, &manifest)?;
                publish_payload(&partial, &payload)?;
                meta["complete"] = true.into();
                meta["payload_file"] = payload.to_string_lossy().to_string().into();
                meta.as_object_mut().unwrap().remove("partial_payload_file");
                // Commit raw acquisition metadata before optional TIFF export,
                // so an export error still leaves a reusable captured payload.
                manifest["passes"][index] = meta.clone();
                write_manifest(&mut manifest_file, &manifest)?;
                let mut image = ImageResult {
                    payload,
                    tiff: options.export_tiff.then(|| tiff.clone()),
                    width,
                    height,
                    channels,
                    depth: pass_settings.depth,
                    dpi: pass_settings.dpi,
                    metadata: meta.clone(),
                };
                if options.measure_sharpness && matches!(pass.kind, PassKind::Rgb | PassKind::Gray)
                {
                    if !progress(Progress {
                        phase: "sharpness",
                        pass: index,
                        done: 0,
                        total: 0,
                    }) {
                        return Err(Error::Cancelled);
                    }
                    meta["sharpness"] = serde_json::to_value(sharpness::measure(&image, cancel)?)?;
                    image.metadata = meta.clone();
                    // Preserve measurements even if subsequent TIFF export fails.
                    manifest["passes"][index] = meta.clone();
                    write_manifest(&mut manifest_file, &manifest)?;
                    if cancel.load(Ordering::Relaxed)
                        || !progress(Progress {
                            phase: "saving",
                            pass: index,
                            done: 0,
                            total: 0,
                        })
                    {
                        return Err(Error::Cancelled);
                    }
                }
                if options.export_tiff {
                    if matches!(pass.kind, PassKind::Rgb | PassKind::Gray)
                        && let Some(banding_options) = &options.banding
                    {
                        if cancel.load(Ordering::Relaxed)
                            || !progress(Progress {
                                phase: "banding",
                                pass: index,
                                done: 0,
                                total: 0,
                            })
                        {
                            return Err(Error::Cancelled);
                        }
                        banding::export(&mut image, &tiff, banding_options, cancel)?;
                        meta["banding"] = image.metadata["banding"].clone();
                    } else {
                        image.save_tiff(&tiff)?;
                    }
                    meta["tiff_file"] = tiff.to_string_lossy().to_string().into();
                }
                image.metadata = meta.clone();
                *manifest["passes"]
                    .as_array_mut()
                    .unwrap()
                    .last_mut()
                    .unwrap() = meta;
                write_manifest(&mut manifest_file, &manifest)?;
                match pass.kind {
                    PassKind::Rgb => result.rgb = Some(image),
                    PassKind::Gray => result.gray = Some(image),
                    PassKind::Ir => result.ir = Some(image),
                    PassKind::Thumbnail => result.thumbnail = Some(image),
                }
            }
            manifest["complete"] = true.into();
            Ok(result)
        })();
        if let Err(error) = &result {
            manifest["error"] = error.to_string().into();
            self.close();
        } else {
            self.protocol()?.trace = None;
        }
        manifest["finished_unix"] = now().into();
        match (result, write_manifest(&mut manifest_file, &manifest)) {
            (Err(error), Err(manifest_error)) => {
                log::error!(
                    "Could not record scan failure in {}: {manifest_error}",
                    manifest_path.display()
                );
                Err(error)
            }
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(result), Ok(())) => Ok(result),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Backend, Device, transport::Transport};
    use std::sync::{Arc, Mutex};

    struct CaptureFixture {
        bytes: Vec<u8>,
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl Transport for CaptureFixture {
        fn read(&mut self, size: usize, _: Duration) -> Result<Vec<u8>> {
            let length = size.min(self.bytes.len());
            Ok(self.bytes.drain(..length).collect())
        }
        fn write(&mut self, data: &[u8], _: Duration) -> Result<usize> {
            self.writes.lock().unwrap().push(data.to_vec());
            Ok(data.len())
        }
    }

    fn session() -> (Session, ScanSettings, Arc<Mutex<Vec<Vec<u8>>>>) {
        session_with_settings(ScanSettings {
            rect_mm: [0., 0., 1., 0.1],
            ..Default::default()
        })
    }

    fn session_with_settings(
        settings: ScanSettings,
    ) -> (Session, ScanSettings, Arc<Mutex<Vec<Vec<u8>>>>) {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/v800-identity.json")).unwrap();
        let hex = fixture["extended_identity_hex"].as_str().unwrap();
        let mut bytes = vec![2, 0x12, 2, 0, b'B', b'8'];
        bytes.extend(
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()),
        );
        let [_, _, width, height] = settings.pixels().unwrap();
        let channels = match settings.mode {
            crate::ScanMode::Rgb => 3,
            crate::ScanMode::Gray => 1,
        };
        let payload_bytes =
            width * height * channels * u32::from(settings.depth / 8) * settings.y_oversampling;
        bytes.extend([6; 5]); // reset, parameters and focus; no default LUT upload
        bytes.extend(settings.parameters(false).unwrap());
        bytes.extend([0; 16]);
        bytes.extend([2, 0x12]);
        for word in [0u32, 0, payload_bytes] {
            bytes.extend(word.to_le_bytes());
        }
        if settings.y_oversampling == 1 {
            bytes.extend(std::iter::repeat_n(0x31, payload_bytes as usize));
        } else {
            let stride = width * channels * u32::from(settings.depth / 8);
            for row in 0..height * settings.y_oversampling {
                bytes.extend(std::iter::repeat_n(row as u8, stride as usize));
            }
        }
        bytes.push(0);
        let writes = Arc::new(Mutex::new(Vec::new()));
        let device = Device {
            location: "synthetic".into(),
            name: "simulator".into(),
            vid: 0x04b8,
            pid: 0x0151,
            backend: Backend::Nusb,
        };
        let session = Session::with_transport(
            device,
            Box::new(CaptureFixture {
                bytes,
                writes: writes.clone(),
            }),
            Duration::from_secs(1),
        )
        .unwrap();
        (session, settings, writes)
    }

    #[test]
    fn oversampling_acquires_extra_rows_but_exports_square_pixels_at_both_depths() {
        for mode in [crate::ScanMode::Gray, crate::ScanMode::Rgb] {
            for depth in [8, 16] {
                let settings = ScanSettings {
                    mode,
                    depth,
                    y_oversampling: 3,
                    rect_mm: [0.0, 0.0, 1.0, 1.0],
                    ..Default::default()
                };
                let (mut session, settings, _) = session_with_settings(settings);
                let directory = tempfile::tempdir().unwrap();
                let mut last_progress = (0, 0);
                let mut watermarks = Vec::new();
                let result = session
                    .scan_observed(
                        &settings,
                        &ScanOptions::default(),
                        &directory.path().join("sampled"),
                        &AtomicBool::new(false),
                        &mut |update| {
                            if update.total > 1 {
                                last_progress = (update.done, update.total);
                            }
                            true
                        },
                        Some(&mut |update| {
                            if let CaptureUpdate::Available(done) = update {
                                watermarks.push(done);
                            }
                        }),
                    )
                    .unwrap();
                let image = result.gray.or(result.rgb).unwrap();
                assert_eq!((image.width, image.height, image.dpi), (8, 12, 300));
                let stride =
                    image.width as usize * usize::from(image.channels) * usize::from(depth / 8);
                let expected: Vec<u8> = (0..12)
                    .flat_map(|row| std::iter::repeat_n((row * 3 + 1) as u8, stride))
                    .collect();
                assert_eq!(fs::read(&image.payload).unwrap(), expected);
                assert_eq!(
                    last_progress,
                    (image.expected_bytes() * 3, image.expected_bytes() * 3)
                );
                assert_eq!(watermarks.last(), Some(&image.expected_bytes()));
                assert!(watermarks.iter().all(|done| done % stride as u64 == 0));
                assert_eq!(
                    image.metadata["sampling"]["acquisition_dpi"],
                    serde_json::json!([300, 900])
                );
                let mut decoder =
                    tiff::decoder::Decoder::new(File::open(image.tiff.unwrap()).unwrap()).unwrap();
                assert_eq!(decoder.dimensions().unwrap(), (8, 12));
                assert_eq!(
                    decoder.get_tag_u32(tiff::tags::Tag::ImageLength).unwrap(),
                    12
                );
            }
        }
    }

    #[test]
    fn late_partial_collision_stops_before_configuration_and_does_not_claim_the_file() {
        let (mut session, settings, writes) = session();
        let directory = tempfile::tempdir().unwrap();
        let partial = directory.path().join("capture_1.partial.bin");
        let result = session.scan(
            &settings,
            &ScanOptions::default(),
            &directory.path().join("capture"),
            &AtomicBool::new(false),
            &mut |progress| {
                if progress.phase == "setup" {
                    fs::write(&partial, b"existing file").unwrap();
                }
                true
            },
        );
        assert!(result.is_err());
        assert_eq!(fs::read(&partial).unwrap(), b"existing file");
        assert_eq!(
            *writes.lock().unwrap(),
            vec![b"\x1bI".to_vec(), b"\x1cI".to_vec()]
        );
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.path().join("capture_1.json")).unwrap())
                .unwrap();
        assert!(manifest["passes"][0].get("partial_payload_file").is_none());
    }

    #[test]
    fn tiff_collision_retains_complete_raw_metadata_and_existing_tiff() {
        let (mut session, settings, _) = session();
        let directory = tempfile::tempdir().unwrap();
        let tiff = directory.path().join("capture_1.tiff");
        let result = session.scan(
            &settings,
            &ScanOptions::default(),
            &directory.path().join("capture"),
            &AtomicBool::new(false),
            &mut |progress| {
                if progress.phase == "rgb" {
                    fs::write(&tiff, b"existing TIFF").unwrap();
                }
                true
            },
        );
        assert!(result.is_err());
        assert_eq!(fs::read(&tiff).unwrap(), b"existing TIFF");
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.path().join("capture_1.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["complete"], false);
        assert_eq!(manifest["passes"][0]["complete"], true);
        assert_eq!(manifest["passes"][0]["acquisition_complete"], true);
        assert!(manifest.get("holder_selection").is_none());
        assert!(manifest["passes"][0].get("holder_selection").is_none());
        assert!(manifest["passes"][0].get("tiff_file").is_none());
        assert_eq!(
            fs::read(manifest["passes"][0]["payload_file"].as_str().unwrap()).unwrap(),
            vec![0x31; 48]
        );
    }

    #[test]
    fn invalid_later_pass_creates_no_files_and_sends_no_configuration() {
        let (mut session, settings, writes) = session();
        let directory = tempfile::tempdir().unwrap();
        let options = ScanOptions {
            infrared: true,
            ir_depth: 16,
            ..Default::default()
        };
        assert!(
            session
                .scan(
                    &settings,
                    &options,
                    &directory.path().join("capture"),
                    &AtomicBool::new(false),
                    &mut |_| true
                )
                .is_err()
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        assert_eq!(
            *writes.lock().unwrap(),
            vec![b"\x1bI".to_vec(), b"\x1cI".to_vec()]
        );
    }

    #[test]
    fn holder_choice_and_model_geometry_are_recorded_in_manifest_and_image() {
        use crate::{capabilities::V800_FAMILY, scan::HolderSelection};
        let selection = HolderSelection {
            holder: crate::capabilities::Holder::V800Film35mm,
            frame_format: None,
            frame: 1,
            overage_percent: 5.0,
        };
        let layout = V800_FAMILY.holder(selection.holder).unwrap();
        let rectangle = V800_FAMILY
            .holder_frame(selection.holder, selection.frame, selection.overage_percent)
            .unwrap();
        let settings = ScanSettings {
            source: layout.source,
            rect_mm: rectangle,
            dpi: 25,
            depth: 8,
            ..Default::default()
        };
        let (mut session, settings, _) = session_with_settings(settings);
        let directory = tempfile::tempdir().unwrap();
        let result = session
            .scan(
                &settings,
                &ScanOptions {
                    holder_selection: Some(selection),
                    export_tiff: false,
                    ..Default::default()
                },
                &directory.path().join("capture"),
                &AtomicBool::new(false),
                &mut |_| true,
            )
            .unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&result.manifest).unwrap()).unwrap();
        let provenance = &manifest["holder_selection"];
        assert_eq!(provenance["holder"], "v800-35mm");
        assert_eq!(provenance["frame"], 1);
        assert_eq!(provenance["overage_percent"], 5.0);
        assert_eq!(provenance["layout"], layout.name);
        assert_eq!(provenance["source"], serde_json::json!(layout.source));
        for (field, expected) in [
            ("rect_mm", rectangle),
            (
                "nominal_rect_mm",
                layout.frame_rect(selection.frame, 0.0).unwrap(),
            ),
        ] {
            for (actual, expected) in provenance[field].as_array().unwrap().iter().zip(expected) {
                assert!((actual.as_f64().unwrap() - expected).abs() < 1e-9);
            }
        }
        assert!(
            provenance["overage_semantics"]
                .as_str()
                .unwrap()
                .contains("centered")
        );
        assert_eq!(&manifest["passes"][0]["holder_selection"], provenance);
        // Compare the decoded JSON forms, allowing JSON's float encoding to
        // round-trip in the same way for the manifest and CLI result.
        let output: serde_json::Value =
            serde_json::from_slice(&serde_json::to_vec(&result).unwrap()).unwrap();
        assert_eq!(&output["rgb"]["metadata"]["holder_selection"], provenance);
    }

    #[test]
    fn grayscale_acquisition_exports_single_channel_and_measures_both_depths() {
        use std::fs::File;
        use tiff::decoder::{Decoder, DecodingResult};

        for depth in [8, 16] {
            let settings = ScanSettings {
                mode: crate::ScanMode::Gray,
                depth,
                rect_mm: [0.0, 0.0, 1.0, 1.0],
                ..Default::default()
            };
            let (mut session, settings, writes) = session_with_settings(settings);
            let directory = tempfile::tempdir().unwrap();
            let mut phases = Vec::new();
            let result = session
                .scan(
                    &settings,
                    &ScanOptions {
                        film: "mono".into(),
                        measure_sharpness: true,
                        ..Default::default()
                    },
                    &directory.path().join("mono"),
                    &AtomicBool::new(false),
                    &mut |update| {
                        phases.push(update.phase.to_owned());
                        true
                    },
                )
                .unwrap();
            assert!(result.rgb.is_none());
            assert!(result.ir.is_none());
            let image = result.gray.as_ref().unwrap();
            assert_eq!(
                (image.width, image.height, image.channels, image.depth),
                (8, 12, 1, depth)
            );
            assert_eq!(image.payload.file_name().unwrap(), "mono_1.bin");
            assert_eq!(
                fs::read(&image.payload).unwrap(),
                vec![0x31; 8 * 12 * usize::from(depth / 8)]
            );
            assert_eq!(image.metadata["pass"], "gray");
            assert_eq!(image.metadata["channel_layout"], "gray");
            assert_eq!(image.metadata["settings"]["mode"], "gray");
            assert_eq!(image.metadata["sharpness"]["tenengrad"], 0.0);
            assert_eq!(image.metadata["sharpness"]["variance_of_laplacian"], 0.0);
            assert!(phases.iter().any(|phase| phase == "gray"));
            assert!(phases.iter().any(|phase| phase == "sharpness"));
            assert!(!phases.iter().any(|phase| phase == "rgb" || phase == "ir"));
            let manifest: serde_json::Value =
                serde_json::from_slice(&fs::read(&result.manifest).unwrap()).unwrap();
            assert_eq!(manifest["complete"], true);
            assert_eq!(manifest["passes"][0]["channels"], 1);
            assert_eq!(
                serde_json::to_value(&result).unwrap()["gray"]["channels"],
                1
            );
            let writes = writes.lock().unwrap();
            let parameters = writes.iter().find(|packet| packet.len() == 64).unwrap();
            assert_eq!(
                (parameters[24], parameters[25], parameters[26]),
                (0, depth, 1)
            );
            let mut decoder =
                Decoder::new(File::open(image.tiff.as_ref().unwrap()).unwrap()).unwrap();
            assert_eq!(decoder.colortype().unwrap(), tiff::ColorType::Gray(depth));
            match decoder.read_image().unwrap() {
                DecodingResult::U8(samples) => assert_eq!(samples, vec![0x31; 8 * 12]),
                DecodingResult::U16(samples) => assert_eq!(samples, vec![0x3131; 8 * 12]),
                other => panic!("Unexpected grayscale TIFF samples: {other:?}"),
            }
        }
    }
}
