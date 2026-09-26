# Hardware observations

## Earlier reference project

These observations belong to the original `../epson` reference project on
2026-09-24/25. They are not new hardware tests of this conversion.

- Windows stock Epson `usbscan.sys`, V800 `04b8:0151`, GT-X980/B8, firmware 1.10.
- Identity reports basic DPI 4800, output range 25-12800, 16-bit input/output.
- Reported areas: flatbed 215.9 x 297.18 mm; holder TPU 149.86 x 246.38 mm;
  full-area TPU 203.2 x 254 mm. V800/V850 share the registered family.
- A 100 dpi RGB8 holder preview and small 300 dpi RGB16 regions completed.
  Repeated small RGB scans completed with the 32-line transfer policy.
- TIFF export reproduced raw samples. Complete RGB was preserved when later
  infrared acquisition failed.
- IR8 payloads were obtained, but RGB-to-IR transitions and repeated standalone
  IR failed. IR16 starts were rejected. These results do not prove spectral
  identity or registration, and this library restricts IR to an 8-bit candidate.
- A 3200 dpi RGB16 attempt failed while uploading the R gamma table: the device
  returned STX instead of ACK. No scan-start command was sent. This is covered by
  an offline regression test but is not established as fixed on hardware.
- nusb enumeration worked; nusb acquisition was not tested with that driver.
  Linux/macOS execution, alternate sources, broader high-DPI/full-bed coverage,
  physical lens selection, calibrated gamma, and spectral IR remain unverified.

The real identity bytes are retained as `tests/fixtures/v800-identity.json`.
Other captures stay outside this repository. Simulated tests exercise packet
formatting, validation, fragmented reads, handshakes, deadlines, cancellation,
failed transfers, metadata preservation and TIFF sample fidelity. Those tests
do not establish new device support or resolve the observed hardware failures.

## User-run converted CLI, 2026-09-25

The initial `epscan scan` command used implicit holder-source defaults: 300 dpi,
RGB16, identity LUT, rectangle 0,0,10,10 mm. Setup and parameter readback succeeded;
FS F reported ready, then FS G returned `0292000000000000000000000000`.
No image bytes arrived. The failed manifest and protocol trace remain in
`dist/scan_1.*` on the user's working copy and are excluded from packages.

All command/reply bytes through the FS G request match the successful RGB pass
in the reference's `captures/rust-rgb-ir_1.protocol.jsonl`, which received 79,296
bytes. This does not identify a parameter or command-sequence regression.
That initial implementation stopped without retrying; subsequent hardware
observations below identify a specific start-triggered preparation condition.

The CLI now requires explicit `--source` and `--rect` for both scan and preview.
This prevents an unintended default scan; it is not a fix for the underlying
device rejection. The basic protocol path still reports rejected starts without
claiming captured pixels; recovery requires the additional evidence below.

## Controlled converted-library trials, 2026-09-25

Trials 01-09 completed eight RGB16 acquisitions, including a 3200-DPI 10 x 10 mm
region and a 300-DPI 100 x 100 mm region. The latter two transferred 9,495,360 and
8,333,136 bytes, respectively, with full blocks larger than 64 KiB. One first
scan after a user-reported power cycle failed immediately with FS G 0x92; an
otherwise matching later attempt succeeded. Neither the missing ESC e command
nor an unconditional one-second sleep is established as the cause or solution.

Trial 11, following a user-reported power cycle, a requested 60-second wait,
and restored USB discovery, captured the decisive transition: FS F reported ready,
the first FS G returned `0292000000000000000000000000`, and immediate status
queries reported warmup (`03` in byte 0) before returning to ready (`01`) at the
2.764-second observation. No image bytes arrived and the diagnostic observer
did not retry. Trial 10 found no USB device and issued no scanner commands.

This is direct evidence of start-triggered, device-reported preparation on the
attached GT-X980/B8. It is not evidence of physical LED lamp heating or a fix for
IR mode-entry and unexpected-ACK failures. The implemented model-gated recovery
allows one additional start after the exact rejected header and an observed
warmup-to-ready transition, under the original acquisition deadline.

Trial 12 validates that production path after a fresh user-confirmed power cycle
with VueScan closed: the first start was rejected, status reported warmup for
approximately 2.766 seconds, and the single resumed start completed all 79,296
RGB16 bytes in 12.363 seconds total. Trial 13 repeated the same 300-DPI holder
region and completed in 7.019 seconds with one start and no recovery. This is
one controlled cold-cycle validation on the attached GT-X980/B8 unit, not a
claim covering other firmware, sources or IR. Across acquisition trials
01-09 and 11-13, ten completed and two unrecovered baseline/observation attempts
failed; trial 10 was enumeration only.

Independent TIFF decoding of all nine production outputs, trials 01-07 and
12-13, matched every raw sample byte and manifest dimension. Raw lengths and
SHA-256 hashes also matched. The successful diagnostic harness trial 09 did
not produce a TIFF.

See [the detailed trial matrix and provenance](hardware-debug-2026-09-25.md)
for settings, status bytes, timing limits, and pinned primary-source references.

Two further 300-DPI RGB16 holder scans of the 0,0,10,10 mm region validated the
rebuilt CLI output modes. Both completed at 112 x 118 pixels. Default output
left stdout empty and reported dimensions, DPI, TIFF and sidecar paths on stderr;
`--json` produced a parseable result on stdout with status on stderr. Both runs
retained only TIFF and JSON sidecar files, and neither result advertised a
deleted raw payload. Captures are in the ignored local directory
`captures/output-check-20260925`.

## 35mm holder geometry, 2026-09-25

A full-source 100-DPI preview located the three openings of the empty 35mm
holder. The registered layout provides eighteen approximate 24 x 36 mm frames.
Holder/frame CLI captures completed in all three strips: frame 1 preview,
frame 8 RGB16 at 300 DPI, and frame 18 preview with 10% overage. Metadata recorded
the selected frame and expansion; successful jobs cleaned raw and trace files.

Repeated previews and regular 100/300-DPI scans found a roughly 2 mm difference
between the full preview and a bottom crop; a top crop aligned within one
100-DPI pixel. No global coordinate correction is justified by these results.
An extra RGB8 identity-LUT trial failed during the blue table upload (STX instead
of ACK), followed by a failed identity query. After the user power-cycled the
scanner, regular RGB8 crops at 100 and 300 DPI succeeded with device-default
gamma. See [holder coordinates, numbering and limitations](holders.md) for the
measurement details. These trials do not verify loaded film exposure boundaries.

## Frame range CLI, 2026-09-25

The rebuilt CLI completed `preview --holder v800-35mm --frame "1-2,2" --overage 10`
on the loaded holder in one connection. It acquired frames 1 and 2 once each,
in order, at 100 DPI/RGB8. Both outputs were 104 x 156 pixels and used the default
built-in gamma mode. The combined `--json` result contained both frame records,
with canonical holder metadata and distinct `film_frame01_1` / `film_frame02_1`
paths. Only the TIFF and JSON sidecar for each frame remained after batch cleanup.
Local artifacts are in `captures/batch-frame-check-20260925`.

## Signed overage and sharpness, 2026-09-25

The rebuilt CLI completed `scan --holder v800-35mm --frame "1,8" --overage -30
--dpi 300 --sharpness` on the loaded holder. Both RGB16 images were 192 x 298
pixels; the centered requested region was 16.8 x 25.2 mm. Each score evaluated
56,240 interior pixels and used device-default gamma without LUT uploads.

| Frame | Tenengrad | Variance of Laplacian |
| --- | --- | --- |
| 1 | 0.07310818218 | 0.01198461937 |
| 8 | 0.19562527127 | 0.01231117450 |

A separate scored frame 8 preview with the same -30% crop completed at 100 DPI,
RGB8, 64 x 99 pixels. Human output included both metrics in scientific notation.
All three acquisitions retained TIFF and JSON sidecar files with scores after
normal raw/trace cleanup. Local artifacts are in
`captures/sharpness-check-20260925`.

These trials validate capture and scoring, not the best holder height. The
physical height was not changed or measured and no height label was supplied.
Scores from different frames or resolutions should not be ranked as a height
comparison. Analytic and simulated acquisition tests additionally cover the
kernel values, blur response, equivalent 8/16-bit normalization, cancellation,
score preservation in TIFF metadata, raw-only output and preflight rejection.
See [the repeatable height-comparison workflow](sharpness.md).

## Batch initialization experiment, 2026-09-25

The baseline scanned holder frames 1, 6 and 13 sequentially on the GT-X980/B8
using RGB16, 1200 DPI, device-default gamma and -50% overage. Each requested
region was 12 x 18 mm; each completed image was 560 x 850 pixels and contained
2,856,000 raw bytes. The baseline reset and set focus for every capture.

Omitting only ESC @ failed on frame 6: the focus payload `59` received unexpected
STX `02` instead of ACK `06`. No acquisition request or image data followed, and
frame 13 was not attempted. After a user-confirmed power cycle, a revised trial
also retained the unchanged focus setting and completed all three frames.
Parameter packets/readbacks matched the baseline; raw lengths, SHA-256 hashes
and independently decoded TIFF samples were verified.

Frames 6 and 13 took **40.423 seconds combined with resets versus 40.401 seconds
with retained initialization**, showing no meaningful improvement in this trial.
The reduced setup wait reappeared while waiting for the FS G image header.
The revised trial's first frame exercised the known cold-start warmup recovery,
so its time is not comparable with the baseline first frame. These tests used
device-default gamma and do not validate custom-LUT retention.

The optimization was removed: production resets and sets focus for each frame
again. The final-image-block ACK policy was unchanged throughout; no ACK is sent
after the last image block. Local evidence is retained under
`captures/batch-reset-check-20260925`, using the `baseline`, `reuse` (failed), and
`reuse-focus` (completed) prefixes.

## Long strip transfer timeout, 2026-09-25

The user's `scan --holder v800-35mm --frame "1-18" --overage -50 --dpi 3200
--measure-sharpness --basename position-4` failed during its first strip.
The saved `dist/position-4_strip01_1.json`, `.protocol.jsonl` and `.partial.bin`
are retained unchanged. Sent parameters and readback match the planned
1512 x 26205 RGB16 image. FS G header `0212006e040032030000b0030400` advertises
818 blocks of 290,304 data bytes and a 263,088-byte tail: 237,731,760 bytes.

Exactly 712 full blocks were received and ACKed. Their 206,696,448 image bytes
(22,784 rows) match the saved partial payload. Block 713 then supplied 290,302
bytes followed by one byte, leaving two bytes missing from its expected
290,305-byte data-plus-status response. The read timed out after about 60
seconds. No ACK followed that incomplete block. Total elapsed time was 399.443
seconds, below the 600-second pass deadline; cropping had not begun. The trace
does not distinguish a device-side short response from USB/driver data loss.
Its final one-byte read cannot be identified as a status byte from this trace.

The rebuilt library targets 64 KiB blocks, reducing this request from 32 to
7 rows per block, and reports block number, committed payload bytes and
received/expected block bytes on timeouts. Smaller blocks are a mitigation for
the user to test, **not a hardware-validated fix**. No timeout extension, padding,
incomplete-block ACK or automatic retry was added. The original partial prefix
contains all requested pixels for frames 1-5; frame 6 lies outside it.

An offline recovery extracted frames 1-5 to
`dist/position-4_recovered_frameNN_1.{tiff,json,bin}` with sharpness scores.
Each frame is 1512 x 2268 RGB16 (20,575,296 raw bytes). Independent source-crop
hashing and TIFF decoding verified every recovered sample. The separate
`position-4_recovered_source_1.json` explicitly describes a recovered prefix,
records the original failure and links the unchanged failed capture. Frame 6
was not recovered. Verification details are in the ignored
`.build/recover-position-4/recovery-report_1.json`; no scanner commands were sent.

## NegPy three-strip failure investigation, 2026-09-25

The user reported twelve completed frames followed by an unexpected STX reply
to CAN at 1200 DPI RGB16. NegPy's saved TIFFs confirm RGB16, and its scanner
settings retain the eighteen edited crop rectangles. The failed temporary
protocol trace was removed by NegPy, so the original transfer-stop cause is
unknown. The CAN response alone does not identify it.

Review against Epson's Image Scan transfer state machine identified an invalid
CAN after fatal/not-ready block status. Those statuses end image transfer;
epscan now stops without ACK/CAN and reports status, block and byte counts.
Other cancellation-handshake failures retain their initiating cause. NegPy
preserves failure manifests and protocol traces without retaining image payloads.

The updated library scanned the exact eighteen saved regions at 1200 DPI RGB16
in three acquisitions. All eighteen extracted images matched the shared source
pixels exactly. Acquisition-weighted progress reached approximately 33.51% and
66.75% at the first two strip ends, held during extraction, and reached 100%
after final callbacks. This successful run did not reproduce the original
intermittent device fault. Evidence is in
`captures/negpy-strip-failure-20260925-222808/verification.json`, with source
manifests, traces, extracted payloads and progress events alongside it.
