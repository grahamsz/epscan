// SPDX-License-Identifier: MIT OR Apache-2.0
use epscan::{Backend, ScanOptions, ScanSettings, Session};
use std::{path::Path, sync::atomic::AtomicBool, time::Duration};

fn main() -> epscan::Result<()> {
    let mut scanner = Session::connect(None, Backend::Auto, Duration::from_secs(60))?;
    println!("{}", serde_json::to_string_pretty(&scanner.capabilities)?);
    let settings = ScanSettings::default(); // TPU, 300 dpi, RGB16, 10 x 10 mm
    let cancel = AtomicBool::new(false);
    let capture = scanner.scan(
        &settings,
        &ScanOptions::default(),
        Path::new("captures/example"),
        &cancel,
        &mut |p| {
            eprintln!("{} {}/{}", p.phase, p.done, p.total);
            true // return false to cancel at a complete block boundary
        },
    )?;
    println!("{}", serde_json::to_string_pretty(&capture)?);
    scanner.close(); // also closes when dropped
    Ok(())
}
