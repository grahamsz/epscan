// SPDX-License-Identifier: MIT
//! Acquire on a dedicated thread while the caller exports committed frame rows.
use super::{
    Progress, ScanOptions, ScanPlan, ScanResult,
    crop::{FrameExtraction, extract_available_frame},
    frame::CaptureUpdate,
};
use crate::{Capabilities, Error, Result, ScanSettings, Session};
use std::{
    fs::File,
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::sync_channel,
    },
    thread,
    time::Duration,
};

pub(super) struct StreamBatch<'a> {
    pub settings: &'a ScanSettings,
    pub options: &'a ScanOptions,
    pub basename: &'a Path,
    pub plan: &'a ScanPlan,
    pub targets: &'a [(usize, FrameExtraction)],
    pub caps: &'a Capabilities,
}

struct Update {
    phase: String,
    pass: usize,
    done: u64,
    total: u64,
}

#[derive(Default)]
struct Mailbox {
    source: Option<(ScanResult, File)>,
    progress: Option<Update>,
}

struct CancelOnDrop<'a> {
    cancel: &'a AtomicBool,
    armed: bool,
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.cancel.store(true, Ordering::Relaxed);
        }
    }
}

pub(super) fn capture(
    session: &mut Session,
    batch: &StreamBatch<'_>,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress<'_>) -> bool,
    region_done: &mut dyn FnMut(usize, &ScanResult) -> Result<bool>,
) -> Result<Vec<(usize, ScanResult)>> {
    let pass = &batch.plan.passes[0];
    let stride =
        u64::from(pass.pixels[2]) * u64::from(pass.channels) * u64::from(pass.settings.depth / 8);
    let mut pending = Vec::with_capacity(batch.targets.len());
    for (position, (_, target)) in batch.targets.iter().enumerate() {
        let plan = target.options.plan(&target.settings, batch.caps)?;
        let pixels = plan.passes[0].pixels;
        let end_row = pixels[1]
            .checked_sub(pass.pixels[1])
            .and_then(|top| top.checked_add(pixels[3]))
            .filter(|&end| end <= pass.pixels[3])
            .ok_or_else(|| Error::Invalid("Streaming frame falls outside its strip".into()))?;
        pending.push((u64::from(end_row) * stride, position));
    }
    pending.sort_unstable();

    let mailbox = Mutex::new(Mailbox::default());
    let available = AtomicU64::new(0);
    // Notifications coalesce; the file and watermark hold the data, not a pixel queue.
    let (wake, updates) = sync_channel(1);
    thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let outcome = session.scan_observed(
                batch.settings,
                batch.options,
                batch.basename,
                cancel,
                &mut |p| {
                    mailbox.lock().unwrap().progress = Some(Update {
                        phase: p.phase.to_owned(),
                        pass: p.pass,
                        done: p.done,
                        total: p.total,
                    });
                    let _ = wake.try_send(());
                    !cancel.load(Ordering::Relaxed)
                },
                Some(&mut |update| {
                    match update {
                        CaptureUpdate::Started { source, reader } => {
                            mailbox.lock().unwrap().source = Some((*source, reader));
                        }
                        CaptureUpdate::Available(bytes) => {
                            available.store(bytes, Ordering::Release)
                        }
                    }
                    let _ = wake.try_send(());
                }),
            );
            let _ = wake.try_send(());
            outcome
        });
        let mut guard = CancelOnDrop {
            cancel,
            armed: true,
        };
        let output = (|| {
            let mut source = None;
            let mut results = Vec::with_capacity(pending.len());
            let mut next = 0;
            loop {
                // Observe completion before taking the final mailbox/watermark snapshot.
                let finished = worker.is_finished();
                let update = {
                    let mut mailbox = mailbox.lock().unwrap();
                    if let Some(started) = mailbox.source.take() {
                        source = Some(started);
                    }
                    mailbox.progress.take()
                };
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                if let Some(update) = update
                    && !progress(Progress {
                        phase: &update.phase,
                        pass: update.pass,
                        done: update.done,
                        total: update.total,
                    })
                {
                    return Err(Error::Cancelled);
                }
                let received = available.load(Ordering::Acquire);
                if let Some((source, reader)) = &source {
                    while next < pending.len() && pending[next].0 <= received {
                        let (index, target) = &batch.targets[pending[next].1];
                        let result = extract_available_frame(
                            source, batch.plan, target, batch.caps, cancel, progress, reader,
                            received,
                        )?;
                        if !region_done(*index, &result)? || cancel.load(Ordering::Relaxed) {
                            return Err(Error::Cancelled);
                        }
                        results.push((*index, result));
                        next += 1;
                    }
                }
                if finished {
                    return Ok(results);
                }
                let _ = updates.recv_timeout(Duration::from_millis(20));
            }
        })();
        if output.is_err() {
            cancel.store(true, Ordering::Relaxed);
        }
        let acquired = worker.join().unwrap_or_else(|_| {
            Err(Error::Protocol(
                "Scanner acquisition thread panicked".into(),
            ))
        });
        guard.armed = false;
        let results = output?;
        acquired?;
        if results.len() != pending.len() {
            return Err(Error::Protocol(
                "Acquisition omitted a completed frame".into(),
            ));
        }
        Ok(results)
    })
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
