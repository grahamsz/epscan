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
The V800 family registers 35 mm strips, mounted slides, 4 x 5 inch sheets and
medium-format film; see [holder calibration](holders.md).

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

## Current compatibility tiers

| Models | Epson USB product ID | Status |
| --- | --- | --- |
| V800/V850 | `0151` | GT-X980/B8 tested locally |
| V700/V750 | `012c` | Provisional GT-X900/B8 RGB and grayscale, 8/16-bit |
| V500 | `0130` | Discovery only; epkowa interpreter required |
| V550 | `013b` | Discovery only; epkowa interpreter required |
| V600 | `013a` | Discovery only; epkowa interpreter required |

All use vendor ID `04b8`. IDs and backend classification come from pinned SANE
`doc/descriptions/epson2.desc`; see [sources](sources.md). The V700/V750 profile
is explicitly provisional: no identity fixture or local hardware validation
exists. Runtime identity and command-level checks still apply. It enables
flatbed, film-holder and film-area-guide sources with identity-bounded output
limits and a 9600-DPI Y sampling cap from Epson specifications. IR, V800 holder
layouts and V800-specific warmup recovery are not inherited. Validate a small
region before relying on this profile for a full scan.

V500/V550/V600 discovery does not claim scan support. Session opening rejects
these devices with an interpreter explanation before sending protocol commands.
Adding their USB IDs cannot implement the missing interpreter. No extra language
or runtime was added. Hardware fixtures remain required before promoting a
provisional profile to tested support.
