//! Direct Epson ESC/I acquisition with interchangeable USB transports.
//!
//! ```no_run
//! use epscan::{Backend, ScanOptions, ScanSettings, Session};
//! use std::{path::Path, sync::atomic::AtomicBool, time::Duration};
//!
//! let mut scanner = Session::connect(None, Backend::Auto, Duration::from_secs(60))?;
//! let result = scanner.scan(
//!     &ScanSettings::default(), &ScanOptions::default(), Path::new("captures/film"),
//!     &AtomicBool::new(false), &mut |_| true,
//! )?;
//! println!("{}", result.manifest.display());
//! # Ok::<(), epscan::Error>(())
//! ```
pub mod capabilities;
pub mod device;
pub mod error;
pub mod protocol;
#[cfg(feature = "python")]
pub mod python;
pub mod scan;
pub mod session;
pub mod transport;

pub use capabilities::{Holder, HolderLayout, ScanMode};
pub use device::{Backend, Device, list_devices};
pub use error::{Error, Result};
pub use protocol::{Capabilities, Gamma, ScanSettings, Source};
pub use scan::{PassKind, PlannedPass, Progress, ScanOptions, ScanPlan, ScanResult};
pub use session::{Session, image::ImageResult};

#[cfg(test)]
mod tests;
