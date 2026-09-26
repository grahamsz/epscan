// SPDX-License-Identifier: MIT OR Apache-2.0
use crate::{Error, Result, protocol::hex};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub fn suffix(path: &Path, text: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(text);
    PathBuf::from(name)
}
pub(super) fn reserve(base: &Path) -> Result<(PathBuf, File, PathBuf)> {
    for index in 1..=u32::MAX {
        let stem = suffix(base, &format!("_{index}"));
        let all = [
            ".tiff",
            "_raw.tiff",
            "_banding.png",
            "_IR.tiff",
            "_thumbnail.tiff",
            ".bin",
            "_IR.bin",
            "_thumbnail.bin",
            ".partial.bin",
            "_IR.partial.bin",
            "_thumbnail.partial.bin",
            ".protocol.jsonl",
        ];
        if all.iter().any(|s| suffix(&stem, s).exists()) {
            continue;
        }
        let path = suffix(&stem, ".json");
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((stem, file, path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::Invalid("No free capture number".into()))
}
pub(super) fn write_manifest(file: &mut File, manifest: &serde_json::Value) -> Result<()> {
    // Serialize before touching the reserved file so serialization failures
    // cannot erase the last successfully persisted state.
    let mut bytes = serde_json::to_vec_pretty(manifest)?;
    bytes.push(b'\n');
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Publish a completed payload without ever replacing an existing path.
/// A hard link avoids a second copy on normal local filesystems. The bounded
/// copy fallback also works on filesystems that do not support hard links.
pub(super) fn publish_payload(partial: &Path, payload: &Path) -> Result<()> {
    if std::fs::hard_link(partial, payload).is_err() {
        let mut source = File::open(partial)?;
        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(payload)?;
        std::io::copy(&mut source, &mut destination)?;
        destination.sync_all()?;
    }
    // A cleanup failure does not invalidate the durable published data. Keep
    // both names and report it; neither path is overwritten on a later scan.
    if let Err(error) = std::fs::remove_file(partial) {
        log::warn!(
            "Captured payload is complete, but could not remove {}: {error}",
            partial.display()
        );
    }
    Ok(())
}
pub(super) fn sha256(path: &Path) -> Result<String> {
    let mut f = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservation_preserves_existing_capture_and_failed_manifest() {
        let directory = tempfile::tempdir().unwrap();
        let basename = directory.path().join("capture");
        let raw = directory.path().join("capture_1.bin");
        let manifest = directory.path().join("capture_2.json");
        std::fs::write(&raw, b"original samples").unwrap();
        std::fs::write(&manifest, b"failed capture").unwrap();
        let (stem, _, reserved) = reserve(&basename).unwrap();
        assert_eq!(stem, directory.path().join("capture_3"));
        assert_eq!(reserved, directory.path().join("capture_3.json"));
        assert_eq!(std::fs::read(raw).unwrap(), b"original samples");
        assert_eq!(std::fs::read(manifest).unwrap(), b"failed capture");
    }

    #[test]
    fn completed_payload_publication_never_replaces_a_late_collision() {
        let directory = tempfile::tempdir().unwrap();
        let partial = directory.path().join("capture.partial.bin");
        let payload = directory.path().join("capture.bin");
        std::fs::write(&partial, b"new capture").unwrap();
        std::fs::write(&payload, b"existing capture").unwrap();
        assert!(publish_payload(&partial, &payload).is_err());
        assert_eq!(std::fs::read(&payload).unwrap(), b"existing capture");
        assert_eq!(std::fs::read(&partial).unwrap(), b"new capture");
        let published = directory.path().join("free.bin");
        publish_payload(&partial, &published).unwrap();
        assert_eq!(std::fs::read(published).unwrap(), b"new capture");
        assert!(!partial.exists());
    }

    #[test]
    fn reservation_preserves_optional_banding_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().join("film");
        let raw = directory.path().join("film_1_raw.tiff");
        let signal = directory.path().join("film_2_banding.png");
        std::fs::write(&raw, b"existing raw").unwrap();
        std::fs::write(&signal, b"existing signal").unwrap();
        let (stem, _, _) = reserve(&base).unwrap();
        assert_eq!(stem, directory.path().join("film_3"));
        assert_eq!(std::fs::read(raw).unwrap(), b"existing raw");
        assert_eq!(std::fs::read(signal).unwrap(), b"existing signal");
    }
}
