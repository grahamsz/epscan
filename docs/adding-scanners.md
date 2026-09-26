# Adding Epson scanners

Start in `src/capabilities.rs`. Its `MODELS` entries define USB identification,
ESC/I identity aliases and command levels, supported source profiles, output
modes/depths, preview defaults, and transfer quirks. Both USB backends discover
through this registry. Identity replies determine actual geometry and further
restrict the registered limits; a USB match alone does not authorize a scan.

The profile declares `rgb_mode`, optional `gray_mode`, and optional
`infrared_mode` independently. Each defines its wire value, channel count and
supported depths. Omit `gray_mode` when grayscale support is unknown; the same
wire mode byte in an infrared profile does not establish visible grayscale support.

Holder layouts also live in `src/capabilities.rs`, under each model's `holders`.
Each layout names its source and ordered rectangles in that source's millimetre
coordinates. Measure a full-holder preview, document the numbering and placement
assumptions, and test every frame against the device's advertised area before
registering a holder. `ScannerModel::holder_frame` adjusts a selected rectangle
symmetrically: positive overage expands it and negative overage crops it.
Percentages must be finite and greater than -100. Ordinary scan validation rejects
out-of-bounds regions rather
than clipping them. Holder identifiers include the scanner family so that
different models can register different layouts for the same film format.
Only `v800-35mm` (`Holder::V800Film35mm`), the approximate three-strip V800 layout,
is currently registered; see [holder calibration](holders.md).

1. Capture the new device's identity/status with known-safe protocol tooling.
   Record the transport, firmware and provenance; do not guess support from a
   similar retail name or shared USB product ID.
2. Add a model entry and its source profiles. Keep model-specific wire values,
   supported depths, optical ratings and limits in this file. An unsupported
   source should be absent, not mapped silently onto another source.
   Enable start-after-warmup recovery only with evidence for that model; a
   generic fatal scan-start response must not imply permission to retry.
3. Add a sanitized identity fixture under `tests/fixtures/` and tests of model
   matching, valid and invalid settings, exact parameter packets, readback and
   transfer termination. Reject unknown identities and command levels.
4. If the scanner requires a new ESC/I dialect, implement that protocol/session
   support explicitly. A new table entry cannot implement an unrecognized wire
   format. Keep transport code concerned with byte I/O.
5. Validate small RGB regions on hardware before claiming wider support. Record
   source, depth, DPI, effective geometry, raw byte count and sample-preserving
   TIFF checks. Verify high DPI, alternate sources and IR independently.

`Source::optics()` and low-level `ScanSettings::parameters()` are convenience
methods for the initial V800-family profile. Generic callers should use the
connected `Capabilities::scanner_model()` profile and the model-aware settings
methods. `Session::scan` and `ScanOptions::plan` already do this.

Manufacturer optical DPI, requested output DPI and measured resolving power
are separate quantities. Focus ACK/readback does not establish which lens is
physically active. Keep unverified features explicitly marked experimental.
