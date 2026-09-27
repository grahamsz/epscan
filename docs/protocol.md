# ESC/I wire contract used by this implementation

Scope: USB 04b8:0151, GT-X980, command level B8. These are protocol facts
researched from the sources listed in [sources.md](sources.md) and checked
against captured device responses. This is an original implementation, not
a claim of independent discovery or a clean-room process.

## Commands

| Bytes | Exchange |
| --- | --- |
| 1b 49 (ESC I) | Four-byte STX/status/u16-LE-length header, then identity payload |
| 1c 49 (FS I) | 80-byte extended identity |
| 1c 46 (FS F) | 16-byte scanner status |
| 1b 40 (ESC @) | Initialize settings, ACK |
| 1c 53 (FS S) | Read 64 current parameter bytes |
| 1c 57 (FS W) | ACK, send 64 parameters, ACK |
| 1b 70 (ESC p) | ACK, focus byte, ACK; 64 glass, 89 holder at 2.5 mm |
| 1b 7a (ESC z) | ACK, channel letter plus 256 LUT bytes, ACK |
| 1b 23 (ESC #) | ACK, 32-byte IR challenge response, ACK expected |
| 1c 47 (FS G) | Start extended acquisition; 14-byte transfer header |

ACK is 06; NAK is 15. A complete nonfinal image block is acknowledged with
06; CAN (18) takes its place to cancel, then ACK is expected. No ACK is sent
after the final image block. Short USB reads are accumulated; short writes,
unexpected replies and inconsistent lengths are errors.

The IR challenge response is the first 32 bytes of FS S XOR this protocol
constant (byte for byte):

```text
ca fb 77 71 20 16 da 09 5f 57 09 12 04 83 76 77
3c 73 9c be 7a e0 52 e2 90 0d ff 9a ef 4c 2c 81
```

The constant is required wire data observed in SANE's `esci_enable_infrared`.
The implementation does not ship that function. An ACK to this exchange does
not prove that V800 IR is ready, spectrally correct or repeatable.

## Identity and parameters

FS I: command level bytes 0..2, basic/min/max DPI u32 LE at 4/8/12;
maximum width at 16; flatbed dimensions at 20/24; TPU dimensions at 36/40;
IR advertisement in byte 44 bit 1; model at 46..62, firmware 62..66;
input/output depth bytes 66/67; TPU2 dimensions at 68/72.
Advertised dimensions use basic-DPI units. Maximum DPI is not optical resolution.

FS W fields, with unlisted bytes zero:

| Offset | Size | Meaning / request |
| --- | --- | --- |
| 0, 4 | u32 LE each | X/Y requested DPI |
| 8, 12, 16, 20 | u32 LE each | x, y, width, height at requested DPI |
| 24 | byte | RGB byte-interleaved 13 hex; visible grayscale and IR candidate 00 |
| 25 | byte | 8 or 16 output bits |
| 26 | byte | flatbed 0; primary TPU 1; IR TPU 3; TPU2/8x10 5 |
| 27 | byte | normal 0; preview speed 1 |
| 28 | byte | Rows per transfer block, derived from packed row size and model transfer policy |
| 29 | byte | built-in gamma 2 (default); custom gamma table 3 (opt-in) |
| 30, 31 | byte | brightness 0; colour correction off 0 |
| 32, 33 | byte | halftoning off 1; threshold 128 |
| 34..38 | bytes | segmentation, sharpening, mirroring, film processing, lamp mode all zero |

Custom LUTs are R/G/B tables with entries 0 through 255. They are not a
measurement of the firmware's effective tone response. Film processing remains
positive/un-inverted for every user film label. Calibration/shading is not
disabled or replaced. Firmware auto gain/exposure behavior has not been measured.

`ScanSettings::mode` selects RGB (default) or visible grayscale. Grayscale uses
one channel at 8 or 16 bits and retains the ordinary source option; it never
sends the IR-enable challenge. An infrared pass remains a separate acquisition
with IR source option 3. Previews and thumbnails preserve the selected visible
mode. TIFFs and per-frame sharpness use the captured grayscale samples directly.

Coordinates use nearest-pixel rounding and width truncation to eight columns,
with effective geometry saved. The V800 profile requests at most 32 rows per
block, targeting 64 KiB including the status byte. The line count is calculated
from width, channel count and sample depth; at least one row is requested even
when that row exceeds the target. Small captures still use 32 rows. For a
1512-column RGB16 strip, this requests 7 rows (63,504 data bytes) instead of
32 rows (290,304 data bytes), without changing image geometry or samples.

With `ScanSettings::y_oversampling > 1`, `dpi` still describes square output
pixels. FS W requests X DPI=`dpi`, Y DPI=`dpi * y_oversampling`; Y offset and
height are the rounded square pixel values multiplied by that factor. This
keeps every cropped output row on the same acquisition sampling grid. The wire
transfer contains the multiplied row count. A bounded writer averages each
group before publishing the square packed payload; output metadata explicitly
records this sample transform, while progress uses wire transfer counts.
Only fully written average groups contribute to the streaming frame watermark.
The V800/V850 carriage sampling cap is 9600 DPI from Epson's hardware
specifications, intersected with connected-device and source limits. Its
12800 DPI output maximum is not used as evidence of additional physical samples.

This smaller-block policy is an **unverified mitigation**, following a long
strip transfer that stalled two bytes short within a block. It is not a proven
fix or a documented USB transfer limit. Earlier 16-line experiments had mixed
results before startup preparation was understood. See the
[failure evidence](hardware-validation.md#long-strip-transfer-timeout-2026-09-25)
and [research sources](sources.md). No SANE model-specific sizing code is used.

Setup for every capture: ESC @, optional FS S / IR challenge, FS W parameters, focus, optional
custom LUTs, FS S readback, FS F status, then FS G. Fields 0..38 are compared
against readback. IR requests select only the primary TPU.

The distinct X/Y resolution fields provide a lead for VueScan-style Epson
single-pass multisampling: acquire additional Y rows and average row groups.
The current library uses equal X/Y DPI and does not implement this yet. See
[the source notes](sources.md);
absence of a dedicated sample-count command is not evidence against it.

The user's [VueScan V800 log observations](sources.md) now
confirms 3200 x 9600 mono16 acquisition with the samples UI set to 16, and
3:1 Y reduction in the saved TIFF geometry. Changing red analog gain from
1.0 to 2.6 changes only the red ESC z table among the logged device settings;
it does not establish a physical exposure command. Our library has not yet
reproduced these unequal-DPI acquisitions.

Source/focus requests express the intended optical path; they are not yet a
verified physical lens readback. The holder path is intended for the nominal
6400-dpi lens, and the wider glass/8x10 path for 4800 dpi. Manufacturer ratings,
requested DPI, basic coordinate DPI and measured optical resolution are separate
metadata concepts. See [the source and optics notes](../README.md).

FS F byte 0: 80 fatal, 40 busy, 02 warming up, 01 warmup cancellation supported.
Byte 2: 80 TPU installed, 40 TPU enabled, 20 TPU error, 02 cover open. Bits are
hexadecimal. A ready response before FS G does not guarantee immediate start
acceptance. These FS F bits must not be decoded as flags in a different reply.

## Observed start-triggered preparation

On 2026-09-25, trial 11 on GT-X980/B8 showed ready FS F, followed by the complete
FS G rejection `0292000000000000000000000000`, then FS F with warmup set
(`0300c000000000000080000000000000`). Status-only observation saw warmup clear
and ready return at 2.764 seconds. This followed a user-reported power cycle and
restored USB discovery; the exact power-on interval was not measured.
The experiment sent no second FS G and captured no pixels.
This confirms a device-reported preparation transition after start, not a
physical LED lamp-warmup mechanism. See [the hardware investigation](hardware-debug-2026-09-25.md).

The bounded production recovery is enabled through the model
profile: consume the exact 0x92 header with zero block size/count/tail, observe
warmup, wait for healthy ready status, and allow at most one additional FS G
under the original pass deadline. No reset or setting changes are part of that
path. Other rejected headers, no warmup indication, status faults, timeouts,
cancellation, and another rejection remain failures. Healthy readiness requires
no fatal, TPU-error, lid-open or busy flag. Low-level `Esci::acquire` sends only
one start; sessions use `acquire_with_policy` with the registered model's
`allow_start_warmup_recovery` policy. Successful transfers record `start_attempts`
and, when recovery occurred, `start_recovery` with the rejected header, status
observations and elapsed preparation time.

Production trial 12 verified this path after a fresh user-confirmed power cycle:
the exact rejection, reported warmup clearing at approximately 2.766 seconds,
one resumed start, and all 79,296 advertised RGB16 bytes. Trial 13 repeated the
same settings successfully with one start and no recovery. These results cover
one cold cycle of this unit and geometry; they do not establish reliable IR
mode entry or other firmware/source behavior.

## Image transfer

FS G reply: STX at byte 0, status at byte 1, then u32 LE block length at 2,
number of full blocks at 6, final partial length at 10. Total must exactly equal
width * height * channels * bytes/sample. Blocks larger than 64 MiB are rejected.
This deliberately fails on unknown padding/layout rather than guessing a stride.

Each block contains its pixel bytes and one trailing status byte. RGB is packed
R,G,B; 16-bit values are little-endian. Data streams to disk before TIFF encoding.
TIFF samples were compared byte-for-byte against real payload files. With Y
oversampling off, no resampling is applied. Pixel rotation, inversion and RGB/IR
registration are not applied.

On 2026-09-26, small empty-holder captures on the GT-X980 verified RGB16 at
3200 x 9600 acquisition DPI (3x Y sampling) and grayscale16 at 4800 x 9600
(2x). Parameter readback matched both requests. The RGB batch delivered two
248 x 252 square-pixel frames plus their pre-banding originals; grayscale
delivered 376 x 378 pixels. This verifies transfer geometry and output, not
image-quality improvement. Local evidence is in
`captures/y-sampling-20260926/verification.json`.

A trailing fatal (80) or not-ready (40) status ends the device's image transfer,
even when the advertised image has more blocks. The host sends neither ACK nor
CAN in that case and reports the original status with the block and byte counts.
The received block remains in the partial payload for diagnosis, but it does not
advance successful scan progress. Bit 10 is a device cancellation request; on a
healthy nonfinal block it is handled by CAN/ACK. These image status meanings are
distinct from the FS F scanner-status fields above. This termination rule is
documented by Epson's pinned Image Scan `start-extended-scan.cpp` implementation
and its `action.hpp` CAN contract (see [sources.md](sources.md)).

A bad header, timeout, disconnect or unexpected reply closes the session.
Cancellation and disk-write failures send CAN only at a known nonfinal ACK
boundary while the scanner has not reported a fatal/not-ready status. If that
cancellation handshake also fails, the error retains the initiating cancellation,
deadline or disk failure and the failed handshake as context. No further command
is sent after a partial-block transport error. A blocked synchronous read is
bounded by its I/O timeout, not instantaneously interrupted by Ctrl+C.

## Known IR gap

SANE's IR option and output-frame declaration are guarded by `SANE_FRAME_IR`.
Its source notes primary-TPU IR testing on GT-X800, not GT-X980. The default IR
mode entry uses one-bit depth; that is not evidence for V800 16-bit operation.

Our IR8 payloads are real transfers, but paired starts return FS G 0x92 and a
standalone repeat returned 02 where the IR challenge ACK was expected. That
left later identity reads out of sync. Ten seconds between passes did not fix
the start failure. No undocumented recovery command is used. The narrowly scoped
RGB preparation finding above does not establish reliable IR mode entry or
repeatability. The missing step is a validated mode-entry/exit and start sequence,
plus proof of the candidate channel's spectral identity. See
[reference hardware observations](hardware-validation.md).
