use crate::{
    Error, Result, ScanOptions, Session,
    protocol::*,
    session::{esci::Esci, image::*},
    transport::{Backend, Device, Transport},
};
use std::{
    fs,
    io::Cursor,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

struct Sim {
    input: Vec<u8>,
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
    fragment: usize,
    closed: Arc<AtomicBool>,
    short_write: bool,
}
impl Transport for Sim {
    fn read(&mut self, size: usize, _: Duration) -> Result<Vec<u8>> {
        let n = size.min(self.fragment).min(self.input.len());
        Ok(self.input.drain(..n).collect())
    }
    fn write(&mut self, b: &[u8], _: Duration) -> Result<usize> {
        self.writes.lock().unwrap().push(b.into());
        Ok(if self.short_write { 0 } else { b.len() })
    }
}
impl Drop for Sim {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
    }
}
fn sim(input: Vec<u8>, fragment: usize) -> (Sim, Arc<Mutex<Vec<Vec<u8>>>>) {
    let writes = Arc::new(Mutex::new(Vec::new()));
    (
        Sim {
            input,
            writes: writes.clone(),
            fragment,
            closed: Arc::new(AtomicBool::new(false)),
            short_write: false,
        },
        writes,
    )
}
fn bytes(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}
fn fixture() -> Vec<u8> {
    let v: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/v800-identity.json")).unwrap();
    bytes(v["extended_identity_hex"].as_str().unwrap())
}
fn header(block: u32, count: u32, tail: u32) -> Vec<u8> {
    let mut h = vec![2, 0x12];
    for n in [block, count, tail] {
        h.extend(n.to_le_bytes());
    }
    h
}
fn identity_wire() -> Vec<u8> {
    let mut w = vec![2, 0x12, 2, 0, b'B', b'8'];
    w.extend(fixture());
    w
}

#[test]
fn real_capability_fixture_is_not_simulated_capture() {
    let c = Capabilities::parse(&fixture()).unwrap();
    assert_eq!(c.model, "GT-X980");
    assert_eq!(c.basic_dpi, 4800);
    assert_eq!(c.max_dpi, 12800);
    assert!(c.infrared_advertised);
    assert_eq!(c.area_mm(Source::Transparency), [149.86, 246.38]);
}
#[test]
fn malformed_identity() {
    assert!(Capabilities::parse(&[0; 79]).is_err());
    assert!(Capabilities::parse(&[0; 80]).is_err());
}
#[test]
fn packets_match_source_offsets() {
    let s = ScanSettings::default();
    assert_eq!(s.pixels().unwrap(), [0, 0, 112, 118]);
    let p = s.parameters(false).unwrap();
    assert_eq!(
        &p[..24],
        bytes("2c0100002c01000000000000000000007000000076000000")
    );
    assert_eq!(
        &p[24..38],
        &[0x13, 16, 1, 0, 32, 2, 0, 0, 1, 128, 0, 0, 0, 0]
    );
    let ir = s.parameters(true).unwrap();
    assert_eq!((ir[24], ir[25], ir[26]), (0, 16, 3));
}
#[test]
fn token_known_vector() {
    assert_eq!(
        infrared_token(&(0..64).collect::<Vec<_>>())
            .unwrap()
            .as_slice(),
        bytes("cafa75722413dc0e575e0319088e78782c628ead6ef544f58814e581f351329e")
    );
    assert!(infrared_token(&[0; 32]).is_err());
}

#[test]
fn default_gamma_skips_uploads_and_custom_gamma_remains_opt_in() {
    assert_eq!(Gamma::default(), Gamma::DeviceDefault);
    let defaults: ScanSettings = serde_json::from_str("{}").unwrap();
    assert_eq!(defaults.gamma, Gamma::DeviceDefault);
    for (gamma, selector, acks) in [
        (ScanSettings::default().gamma, 2, 5),
        (Gamma::IdentityLut, 3, 11),
    ] {
        let settings = ScanSettings {
            gamma,
            ..Default::default()
        };
        let parameters = settings.parameters(false).unwrap();
        assert_eq!(parameters[29], selector);
        let mut input = vec![6; acks];
        input.extend(parameters);
        let (transport, writes) = sim(input, 7);
        let mut protocol = Esci::new(Box::new(transport), Duration::from_secs(1)).unwrap();
        protocol.configure(&settings, false).unwrap();
        let writes = writes.lock().unwrap();
        let tables: Vec<_> = writes
            .windows(2)
            .filter(|pair| pair[0] == b"\x1bz")
            .map(|pair| &pair[1])
            .collect();
        if gamma == Gamma::DeviceDefault {
            assert!(tables.is_empty());
            assert_eq!(writes.len(), 6); // configuration and parameter readback only
        } else {
            assert_eq!(tables.len(), 3);
            for (table, color) in tables.iter().zip(b"RGB") {
                assert_eq!(table[0], *color);
                assert_eq!(&table[1..], &(0u8..=255).collect::<Vec<_>>());
            }
        }
    }
}

#[test]
fn gamma_payload_stx_names_failure_and_stops_before_scan() {
    // The real 3200-dpi failure: reset, parameter and focus ACKs, then
    // ESC z ACK followed by 02 instead of the R-table payload ACK.
    let mut input = vec![6; 6];
    input.push(2);
    let (transport, writes) = sim(input, 1);
    let mut protocol = Esci::new(Box::new(transport), Duration::from_secs(1)).unwrap();
    let settings = ScanSettings {
        dpi: 3200,
        rect_mm: [10., 10., 100., 100.],
        gamma: Gamma::IdentityLut,
        ..Default::default()
    };
    let error = protocol
        .configure(&settings, false)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("ESC z R identity gamma table payload (257 bytes)"),
        "{error}"
    );
    assert!(error.contains("got 02 (STX"), "{error}");
    let writes = writes.lock().unwrap();
    assert_eq!(writes.len(), 7);
    assert_eq!(writes.last().unwrap()[0], b'R');
    assert!(!writes.iter().any(|command| command == b"\x1cG"));
}
#[test]
fn geometry_rejects_invalid_and_rounds() {
    for rect in [
        [-1., 0., 10., 10.],
        [0., 0., 0., 10.],
        [0., 0., f64::NAN, 10.],
        [0., 0., 0.01, 10.],
    ] {
        let s = ScanSettings {
            rect_mm: rect,
            ..Default::default()
        };
        assert!(s.pixels().is_err());
    }
    let c = Capabilities::parse(&fixture()).unwrap();
    let s = ScanSettings {
        rect_mm: [149., 0., 10., 10.],
        ..Default::default()
    };
    assert!(s.validate(&c, false).is_err());
}
#[test]
fn ir_rejects_wrong_source() {
    let c = Capabilities::parse(&fixture()).unwrap();
    let s = ScanSettings {
        source: Source::Flatbed,
        ..Default::default()
    };
    assert!(s.validate(&c, true).is_err());
}

#[test]
fn film_holder_optics_stay_distinct_from_full_area_guide() {
    #[cfg(feature = "cli")]
    assert!(
        clap::ValueEnum::to_possible_value(&Source::Transparency)
            .unwrap()
            .matches("film-holder", false)
    );
    let holder = ScanSettings::default();
    let guide = ScanSettings {
        source: Source::Transparency8x10,
        ..holder.clone()
    };
    assert_eq!(holder.parameters(false).unwrap()[26], 1);
    assert_eq!(guide.parameters(false).unwrap()[26], 5);
    assert_eq!(holder.source.optics().manufacturer_optical_dpi, 6400);
    assert_eq!(holder.source.optics().focus_command_position, 89);
    assert_eq!(guide.source.optics().manufacturer_optical_dpi, 4800);
    assert_eq!(guide.source.optics().focus_command_position, 64);
    assert!(!holder.source.optics().physical_lens_verified);
    let caps = Capabilities::parse(&fixture()).unwrap();
    let oversized = ScanSettings {
        rect_mm: [0., 0., 170., 10.],
        ..holder
    };
    assert!(oversized.validate(&caps, false).is_err());
    let wide_guide = ScanSettings {
        source: Source::Transparency8x10,
        ..oversized
    };
    assert!(wide_guide.validate(&caps, false).is_ok());
}
#[test]
fn partial_reads_and_termination() {
    let mut wire = header(6, 2, 3);
    wire.extend(b"abcdef\0ghijkl\0mno\0");
    let (t, w) = sim(wire, 2);
    let mut p = Esci::new(Box::new(t), Duration::from_secs(1)).unwrap();
    let mut sink = Vec::new();
    let got = p
        .acquire(
            15,
            &mut sink,
            &AtomicBool::new(false),
            Duration::from_secs(10),
            &mut |_, _| true,
        )
        .unwrap();
    assert_eq!(sink, b"abcdefghijklmno");
    assert_eq!(got.received_bytes, 15);
    assert_eq!(
        *w.lock().unwrap(),
        vec![b"\x1cG".to_vec(), vec![6], vec![6]]
    );
}
#[test]
fn tail_only_never_acknowledged() {
    let mut wire = header(0, 0, 3);
    wire.extend(b"abc\0");
    let (t, w) = sim(wire, 20);
    let mut p = Esci::new(Box::new(t), Duration::from_secs(1)).unwrap();
    p.acquire(
        3,
        &mut Vec::new(),
        &AtomicBool::new(false),
        Duration::from_secs(10),
        &mut |_, _| true,
    )
    .unwrap();
    assert_eq!(w.lock().unwrap().len(), 1);
}
#[test]
fn cancel_sends_can_at_boundary() {
    let mut wire = header(3, 2, 0);
    wire.extend(b"abc\0\x06");
    let (t, w) = sim(wire, 3);
    let mut p = Esci::new(Box::new(t), Duration::from_secs(1)).unwrap();
    assert!(matches!(
        p.acquire(
            6,
            &mut Vec::new(),
            &AtomicBool::new(false),
            Duration::from_secs(10),
            &mut |_, _| false
        ),
        Err(Error::Cancelled)
    ));
    assert_eq!(*w.lock().unwrap(), vec![b"\x1cG".to_vec(), vec![0x18]]);
}
#[test]
fn pre_cancel_does_not_start() {
    let (t, w) = sim(Vec::new(), 3);
    let mut p = Esci::new(Box::new(t), Duration::from_secs(1)).unwrap();
    assert!(matches!(
        p.acquire(
            6,
            &mut Vec::new(),
            &AtomicBool::new(true),
            Duration::from_secs(10),
            &mut |_, _| true
        ),
        Err(Error::Cancelled)
    ));
    assert!(w.lock().unwrap().is_empty());
}
#[test]
fn malformed_transfer_counts() {
    for h in [header(0, 4, 0), header(100_000_000, 0, 6), header(3, 0, 0)] {
        assert!(TransferHeader::parse(&h, 6).is_err());
    }
}
#[test]
fn busy_and_fatal_are_distinct() {
    let mut h = header(3, 1, 0);
    h[1] = 0x40;
    assert!(matches!(TransferHeader::parse(&h, 3), Err(Error::Busy(_))));
    h[1] = 0x80;
    assert!(matches!(
        TransferHeader::parse(&h, 3),
        Err(Error::Protocol(_))
    ));
}
#[test]
fn recorded_ready_status_can_be_followed_by_rejected_scan_start() {
    // User-run epscan scan, 2026-09-25: acknowledged setup, ready FS F,
    // then a fatal FS G header with no image payload or transfer counts.
    let mut wire = bytes("0100c000000000000080000000000000");
    wire.extend(bytes("0292000000000000000000000000"));
    let (transport, writes) = sim(wire, 7);
    let mut protocol = Esci::new(Box::new(transport), Duration::from_secs(1)).unwrap();
    let cancel = AtomicBool::new(false);
    protocol
        .wait_ready(&cancel, Duration::from_secs(1))
        .unwrap();
    let mut payload = Vec::new();
    let error = protocol
        .acquire(
            79_296,
            &mut payload,
            &cancel,
            Duration::from_secs(1),
            &mut |_, _| true,
        )
        .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("Scanner rejected scan start"), "{message}");
    assert!(message.contains("0x92"), "{message}");
    assert!(payload.is_empty());
    assert_eq!(
        *writes.lock().unwrap(),
        vec![b"\x1cF".to_vec(), b"\x1cG".to_vec()]
    );
}
#[test]
fn nak_truncation_and_short_write() {
    let (t, _) = sim(vec![0x15], 3);
    let mut p = Esci::new(Box::new(t), Duration::from_secs(1)).unwrap();
    assert!(matches!(
        p.query(b"\x1cI", 80),
        Err(Error::Unsupported { .. })
    ));
    let (t, _) = sim(vec![1, 2], 3);
    let mut p = Esci::new(Box::new(t), Duration::from_secs(1)).unwrap();
    assert!(p.read_exact(3, false).is_err());
    let (mut t, _) = sim(Vec::new(), 3);
    t.short_write = true;
    let mut p = Esci::new(Box::new(t), Duration::from_secs(1)).unwrap();
    assert!(p.write(b"\x1cI").is_err());
}
#[test]
fn image_status_error_never_succeeds() {
    let mut wire = header(3, 1, 0);
    wire.extend(b"abc\x80");
    let (t, _) = sim(wire, 3);
    let mut p = Esci::new(Box::new(t), Duration::from_secs(1)).unwrap();
    assert!(
        p.acquire(
            3,
            &mut Vec::new(),
            &AtomicBool::new(false),
            Duration::from_secs(10),
            &mut |_, _| true
        )
        .is_err()
    );
}
#[test]
fn status_bits() {
    let mut b = [0; 16];
    b[0] = 0x42;
    b[2] = 0xa2;
    let s = Status::parse(&b).unwrap();
    assert!(s.busy && s.warming_up && s.lid_open && s.transparency_error);
    assert!(Status::parse(&b[..15]).is_err());
}
#[test]
fn read_timeout_is_mapped() {
    struct Timed;
    impl Transport for Timed {
        fn read(&mut self, _: usize, _: Duration) -> Result<Vec<u8>> {
            Err(Error::Timeout("test".into()))
        }
        fn write(&mut self, b: &[u8], _: Duration) -> Result<usize> {
            Ok(b.len())
        }
    }
    let mut p = Esci::new(Box::new(Timed), Duration::from_secs(1)).unwrap();
    assert!(matches!(p.read_exact(3, false), Err(Error::Timeout(_))));
}
#[test]
fn tiff_preserves_rgb16_order_and_samples() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("rgb.bin");
    let tif = dir.path().join("rgb.tiff");
    let bytes = bytes("010002010304ffff0180aa55");
    fs::write(&bin, &bytes).unwrap();
    let result = ImageResult {
        payload: bin,
        tiff: Some(tif.clone()),
        width: 2,
        height: 1,
        channels: 3,
        depth: 16,
        dpi: 300,
        metadata: serde_json::json!({"linearity":"unverified"}),
    };
    result.save_tiff(&tif).unwrap();
    let mut decoder = tiff::decoder::Decoder::new(Cursor::new(fs::read(&tif).unwrap())).unwrap();
    assert_eq!(decoder.dimensions().unwrap(), (2, 1));
    let tiff::decoder::DecodingResult::U16(samples) = decoder.read_image().unwrap() else {
        panic!()
    };
    assert_eq!(samples, vec![1, 258, 1027, 65535, 32769, 21930]);
    assert!(result.save_tiff(&tif).is_err());
}
#[test]
fn gray8_multistrip_and_bad_stride() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("ir.bin");
    let t = d.path().join("ir.tiff");
    let values: Vec<u8> = (0..240).collect();
    fs::write(&p, &values).unwrap();
    let mut r = ImageResult {
        payload: p,
        tiff: Some(t.clone()),
        width: 4,
        height: 60,
        channels: 1,
        depth: 8,
        dpi: 300,
        metadata: serde_json::json!({}),
    };
    r.save_tiff(&t).unwrap();
    let mut dec = tiff::decoder::Decoder::new(Cursor::new(fs::read(t).unwrap())).unwrap();
    let tiff::decoder::DecodingResult::U8(out) = dec.read_image().unwrap() else {
        panic!()
    };
    assert_eq!(out, values);
    r.width = 5;
    assert!(r.save_tiff(&d.path().join("bad.tiff")).is_err());
    assert!(decode_u16_le(&[1]).is_err());
}
#[test]
fn failed_session_releases_transport_and_retains_manifest() {
    let (t, _) = sim(identity_wire(), 20);
    let dropped = t.closed.clone();
    let device = Device {
        location: "synthetic".into(),
        name: "test".into(),
        vid: 0x04b8,
        pid: 0x151,
        backend: Backend::Nusb,
    };
    let mut session = Session::with_transport(device, Box::new(t), Duration::from_secs(1)).unwrap();
    let d = tempfile::tempdir().unwrap();
    assert!(
        session
            .scan(
                &ScanSettings::default(),
                &ScanOptions::default(),
                &d.path().join("scan"),
                &AtomicBool::new(false),
                &mut |_| true
            )
            .is_err()
    );
    assert!(dropped.load(Ordering::Relaxed));
    let m: serde_json::Value =
        serde_json::from_slice(&fs::read(d.path().join("scan_1.json")).unwrap()).unwrap();
    assert_eq!(m["complete"], false);
    assert!(m["error"].is_string());
}

#[test]
fn disk_failure_cancels_at_known_boundary() {
    struct FullDisk;
    impl std::io::Write for FullDisk {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("simulated full disk"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut wire = header(3, 2, 0);
    wire.extend(b"abc\0\x06");
    let (transport, writes) = sim(wire, 2);
    let mut protocol = Esci::new(Box::new(transport), Duration::from_secs(1)).unwrap();
    let outcome = protocol.acquire(
        6,
        &mut FullDisk,
        &AtomicBool::new(false),
        Duration::from_secs(2),
        &mut |_, _| true,
    );
    assert!(matches!(outcome, Err(Error::Io(_))));
    assert_eq!(*writes.lock().unwrap(), vec![b"\x1cG".to_vec(), vec![0x18]]);
}

pub(crate) fn one_rgb_session() -> (Session, ScanSettings) {
    let settings = ScanSettings {
        rect_mm: [0., 0., 1., 0.1],
        ..Default::default()
    };
    let mut wire = identity_wire();
    wire.extend([6; 5]); // reset, parameters and focus; no default LUT upload
    wire.extend(settings.parameters(false).unwrap());
    wire.extend([0; 16]); // ready status
    wire.extend(header(0, 0, 48));
    wire.extend([0x31; 48]);
    wire.push(0);
    let (transport, _) = sim(wire, 7);
    let device = Device {
        location: "synthetic".into(),
        name: "simulator".into(),
        vid: 0x04b8,
        pid: 0x151,
        backend: Backend::Nusb,
    };
    (
        Session::with_transport(device, Box::new(transport), Duration::from_secs(1)).unwrap(),
        settings,
    )
}

#[test]
fn raw_only_scan_can_export_tiff_later() {
    let (mut session, settings) = one_rgb_session();
    let directory = tempfile::tempdir().unwrap();
    let options = ScanOptions {
        export_tiff: false,
        ..Default::default()
    };
    let result = session
        .scan(
            &settings,
            &options,
            &directory.path().join("scan"),
            &AtomicBool::new(false),
            &mut |_| true,
        )
        .unwrap();
    let image = result.rgb.unwrap();
    assert!(image.tiff.is_none());
    assert_eq!(fs::read(&image.payload).unwrap(), vec![0x31; 48]);
    assert!(!directory.path().join("scan_1.tiff").exists());
    image
        .save_tiff(&directory.path().join("later.tiff"))
        .unwrap();
}

#[test]
fn later_pass_failure_keeps_complete_rgb() {
    let (mut session, settings) = one_rgb_session();
    let directory = tempfile::tempdir().unwrap();
    let options = ScanOptions {
        infrared: true,
        settle_time: Duration::ZERO,
        ..Default::default()
    };
    assert!(
        session
            .scan(
                &settings,
                &options,
                &directory.path().join("pair"),
                &AtomicBool::new(false),
                &mut |_| true
            )
            .is_err()
    );
    assert!(directory.path().join("pair_1.tiff").exists());
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.path().join("pair_1.json")).unwrap()).unwrap();
    assert_eq!(manifest["complete"], false);
    assert_eq!(manifest["passes"][0]["complete"], true);
    assert!(session.diagnostics().is_err());
}

#[test]
fn visible_scan_exports_banding_artifacts_without_changing_capture_samples() {
    use crate::scan::banding::BandingOptions;
    let settings = ScanSettings::default();
    let [_, _, width, height] = settings.pixels().unwrap();
    let mut payload = Vec::new();
    for _y in 0..height {
        for x in 0..width {
            let value = (65535.0
                * 0.25
                * (0.045 * (std::f64::consts::TAU * f64::from(x) / 13.25 + 0.4).cos()).exp())
            .round_ties_even() as u16;
            for _ in 0..3 {
                payload.extend(value.to_le_bytes());
            }
        }
    }
    let mut wire = identity_wire();
    wire.extend([6; 5]);
    wire.extend(settings.parameters(false).unwrap());
    wire.extend([0; 16]);
    wire.extend(header(0, 0, payload.len() as u32));
    wire.extend(&payload);
    wire.push(0);
    let (transport, _) = sim(wire, 4096);
    let device = Device {
        location: "synthetic".into(),
        name: "simulator".into(),
        vid: 0x04b8,
        pid: 0x151,
        backend: Backend::Nusb,
    };
    let mut session =
        Session::with_transport(device, Box::new(transport), Duration::from_secs(1)).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let options = ScanOptions {
        banding: Some(BandingOptions {
            save_raw: true,
            save_signal: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut phases = Vec::new();
    let result = session
        .scan(
            &settings,
            &options,
            &directory.path().join("film"),
            &AtomicBool::new(false),
            &mut |p| {
                phases.push(p.phase.to_owned());
                true
            },
        )
        .unwrap();
    let image = result.rgb.unwrap();
    assert_eq!(fs::read(&image.payload).unwrap(), payload);
    assert!(phases.iter().any(|phase| phase == "banding"));
    for filename in ["film_1.tiff", "film_1_raw.tiff", "film_1_banding.png"] {
        assert!(directory.path().join(filename).metadata().unwrap().len() > 0);
    }
    let mut raw = tiff::decoder::Decoder::new(
        fs::File::open(directory.path().join("film_1_raw.tiff")).unwrap(),
    )
    .unwrap();
    let tiff::decoder::DecodingResult::U16(raw_values) = raw.read_image().unwrap() else {
        panic!("16-bit raw TIFF");
    };
    assert_eq!(raw_values, decode_u16_le(&payload).unwrap());
    let mut fixed =
        tiff::decoder::Decoder::new(fs::File::open(image.tiff.as_ref().unwrap()).unwrap()).unwrap();
    let tiff::decoder::DecodingResult::U16(fixed_values) = fixed.read_image().unwrap() else {
        panic!("16-bit corrected TIFF");
    };
    assert_ne!(fixed_values, raw_values);
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(result.manifest).unwrap()).unwrap();
    assert_eq!(manifest["complete"], true);
    let persisted_banding: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&image.metadata["banding"]).unwrap()).unwrap();
    assert_eq!(manifest["passes"][0]["banding"], persisted_banding);
    assert_eq!(image.metadata["banding"]["config"]["save_raw"], true);
    assert_eq!(image.metadata["banding"]["config"]["save_signal"], true);
}
