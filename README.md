# epscan

Rust library, optional CLI, and Python bindings for direct Epson ESC/I USB
acquisition. The first model profile is the Perfection V800/V850 family
(`04b8:0151`, GT-X980/B8).

V700/V750 (`04b8:012c`, GT-X900/B8) has a provisional, hardware-untested
RGB/grayscale profile. V500, V550 and V600 are recognized by USB ID but cannot
scan yet: they require the epkowa interpreter, not the native protocol path.
See [model support and provenance](docs/adding-scanners.md).

This is loosely inspired by the coparalbe [nkscan](https://github.com/activexray/nkscan) project but given that the
hardware is so different a lot gets replaced.


## Build and CLI

```sh
cargo build --release --locked --features cli --bin epscan
cargo install --path . --locked --features cli
epscan list
epscan dump --output diagnostics.json
epscan preview --source film-holder --rect "0,0,149,246" --basename captures/preview
epscan scan --source film-holder --rect "10,30,40,80" --dpi 300 --depth 16 --basename captures/film
epscan scan --source film-holder --rect "10,30,10,10;70,70,10,10" --dpi 3200 --measure-sharpness --basename captures/areas
epscan scan --source film-holder --rect "10,30,40,80" --dpi 3200 --y-oversampling 3 --basename captures/sampled
epscan scan --holder v800-35mm --frame "1-5,8-12" --overage 5 --dpi 3200 --basename captures/film
epscan preview --holder v800-4x5 --frame 1 --basename captures/sheet-preview
epscan scan --holder v800-4x5 --frame 1 --dpi 1200 --basename captures/sheet
```

`--rect` takes one quoted argument containing `x,y,width,height` in millimetres
relative to the selected source. Separate multiple areas with semicolons:
`--rect "10,30,10,10;70,70,10,10"`. Quotes are required to keep the semicolon
inside the argument in PowerShell and other shells. The old four-space-separated
value syntax is no longer accepted.
Coordinates round to pixels; width rounds down to the model's alignment (eight
pixels on V800). Requested and effective geometry are recorded. Both `scan` and
`preview` require either `--source` with `--rect`, or `--holder` with `--frame`;
a bare command prints usage without opening the scanner. `scan` defaults to 300 dpi and RGB16 once those
arguments are provided. `preview` uses the model's 100 dpi/8-bit preset,
also defaulting to RGB. Source selection never silently changes to accommodate a larger area.

Use `--mode gray` (alias `--mode mono`) for true single-channel grayscale
acquisition and grayscale TIFFs. It supports 8- and 16-bit scans, previews,
holder batches, and sharpness measurement. For example:

```powershell
.\epscan.exe scan --holder v800-35mm --frame "1-18" --overage -50 --dpi 3200 --mode gray --film mono --measure-sharpness --basename position-4-mono
```

RGB remains the default. `--film mono` only labels the film; `--mode gray`
selects the scanner's grayscale acquisition mode. Samples remain un-inverted.
Grayscale transfers one third as many image bytes as RGB at the same geometry
and bit depth; mechanical scan time may differ. JSON records `mode: "gray"`
and a `gray` pass/result. The new grayscale path is covered by offline tests;
hardware validation of this implementation is pending.

Both commands combine nearby vertical explicit areas into bounding-box scans:
regions must overlap by half the narrower width and have gaps of at most 10 mm.
Larger gaps, separate columns, and infrared or thumbnail jobs stay independent.
Each output retains its exact requested pixel region and the given order,
including repeated identical rectangles. All areas and passes are validated
before any capture starts. Explicit areas require `--source` and cannot be
combined with `--holder`, `--frame`, or `--overage`. A single area keeps filenames
such as `film_1.tiff`; multiple areas use `film_area01_1.tiff`,
`film_area02_1.tiff`, and separate sidecars. The final capture number advances
to avoid overwriting existing files.

`--holder v800-35mm` selects the Epson V800/V850 35 mm Film Strip Holder and its film
source. Frames 1–6 run down the left strip, 7–12 down the middle, and 13–18 down
the right in the unrotated preview. `--overage 5` adds 5% to both frame dimensions,
centered (2.5% on each edge); `--overage -10` crops each dimension by 10%,
also centered. The value must be finite and greater than -100%; the default is
zero. Holder selection cannot be
combined with `--rect`, and `--overage` requires holder selection. The positions
are approximate: the holder was measured empty, so film placement may shift the
actual images. `--holder v800-4x5 --frame 1` selects the Epson V800/V850
4 x 5 inch Film Holder and its single sheet position. Its measured usable opening
is approximately 94 × 119 mm; the holder mask covers part of the nominal sheet.
All registered holders use the transparency source. The Epson V800/V850 Medium
Format Film Holder is `--holder v800-medium-format`. Select its exposure preset
with `--frame-format 6x4.5`, `6x6` (default), `6x7`, `6x8`, `6x9`, `6x12`, or
`6x17`; the measured opening fits four, three, two, two, two, one, or one nominal
frames respectively. The 35 mm holder also accepts `--frame-format 35mm-half`,
with twelve 24 x 18 mm frames per strip (frames 1-12, 13-24, and 25-36).
Omitting `--frame-format` preserves the original full-frame 35 mm layout.

```powershell
epscan preview --holder v800-medium-format --frame-format 6x6 --frame 1-3
epscan scan --holder v800-medium-format --frame-format 6x7 --frame 1-2 --dpi 2400
epscan scan --holder v800-35mm --frame-format 35mm-half --frame 1-12 --dpi 2400
```

Frame sizes and spacing are starting points: camera gates and loaded-film
positions vary. The medium-format opening was measured empty, so film alignment
has not been verified. Use NegPy's editable preview crops, `--overage` for a
centered size change, or `--rect` for custom size and placement. Incompatible
holder/format combinations and frame numbers are rejected before connecting.
See the [measured holder layouts](docs/holders.md) for dimensions and numbering.

`--frame` accepts individual numbers and inclusive ranges, such as `1-5,8-12`.
Results follow the requested order; duplicate frames are included once. Every frame is
validated before scanning starts. Holder outputs include the position, for
example `film_frame01_1.tiff`, with a separate sidecar for each frame. For either
frame or area batches, a failure stops the batch and keeps its raw data and
traces, including completed captures.
Successful batches use the same cleanup policy as single scans.

For single-pass RGB or grayscale scans and previews, multiple selected frames on the same
registered strip share one acquisition. The 35 mm holder strips are frames 1–6, 7–12,
and 13–18. Each acquisition covers the selected frames' bounding region,
including any gaps; each output is then cropped to its own planned pixel
rectangle with its overage applied. Samples are copied exactly, and sharpness
is measured separately on each resulting frame. IR or thumbnail jobs and
explicit rectangles keep separate acquisitions. Shared captures retain a
`film_strip01_1.json` provenance sidecar alongside the individual frame outputs;
their raw payload and trace are removed only after all frame outputs succeed,
unless `--keep-intermediates` is set. See [holder batching](docs/holders.md).

`--measure-sharpness` records Tenengrad and variance of Laplacian for each main RGB or grayscale
image, including each explicit area in a batch. Use the same regions and scan
settings to compare manually adjusted holder heights, with a distinct basename
such as `captures/position-4` for each setting.
Negative overage can keep holder edges out of the measurement. See the
[sharpness comparison workflow and metric definitions](docs/sharpness.md).

Use `--reduce-banding` to reduce vertical stripes in the exported RGB or grayscale
TIFF. It estimates repeating column variations and their strength across the
unrotated image, then refines signed residuals and applies a pure-sine correction to dark areas of the
negative. The defaults are **100% fitted correction strength**, a full darkness mask below
10% of the original sample range, and a smooth fade to zero at **60% brightness**.
Bright pixels at or above that cutoff stay unchanged. Original orientation, DPI,
dimensions and sample depth are retained, including 16-bit output.

```powershell
epscan scan --holder v800-35mm --frame "1" --dpi 3200 --depth 16 --reduce-banding --save-raw-tiff --save-band-signal --basename captures/film
```

With this example, `film_frame01_1.tiff` is the corrected output,
`film_frame01_1_raw.tiff` preserves the uncorrected samples, and
`film_frame01_1_banding.png` visualizes the signed correction actually applied
after the darkness mask and strength. The PNG is a reduced diagnostic preview;
each panel preserves the image's aspect ratio, with separate R/G/B panels for
color scans. Red indicates positive intensity removed (samples darkened), blue
indicates negative intensity removed (samples brightened), and white means zero.
The fitter is shared with Photoshop in `crates/negative-banding`; TIFFs retain
full source precision. See [current method](docs/banding-correction.md).
JSON metadata records the
configuration, detected bands and output paths. Optional exports must finish
successfully before the CLI removes intermediate raw `.bin` payloads.

Tune `--banding-strength 1.0`, `--banding-dark-full 0.1`, and
`--banding-dark-off 0.6` as fractions from 0 to 1; the full-mask threshold must
be below the cutoff. `--banding-roi "X0,X1,Y0,Y1"` restricts frequency detection
to a half-open region in original output pixels, useful for choosing a smooth
sky or dense area instead of periodic scene detail. Correction still covers the
whole image. These options and the optional exports require `--reduce-banding`,
which also works with `preview` and holder/area batches. It conflicts with
`--raw-only` and `--ir-only`; an additional IR pass is left uncorrected. Inspect
the corrected and raw images when tuning: genuine repeating image detail can
be mistaken for scanner bands. See [the algorithm and limitations](docs/banding-correction.md).

| Source | Placement | Manufacturer optical rating |
| --- | --- | --- |
| `transparency` / `film-holder` | Film in holders | 6400 dpi |
| `transparency-8x10` / `film-area-guide` | Film on glass with area guide | 4800 dpi |
| `flatbed` | Reflective original on glass | 4800 dpi |

These describe the intended optical path. Physical lens selection seems to happen
at hardware discreption. Output DPI is a separate request, limited by both the
model profile and the scanner's identity response.

`scan --help` lists the Epson options. `--thumbnail` adds a preview of the same
region. Successful CLI scans and previews keep TIFF images and JSON metadata by
default. Once the whole job finishes successfully, the CLI removes its raw `.bin`
files and `.protocol.jsonl` trace. Use `--keep-intermediates` to retain those files
alongside the TIFFs. `--raw-only` keeps raw payloads and JSON metadata without
exporting TIFF; combine it with `--keep-intermediates` to keep the protocol trace
too. Failed or cancelled jobs retain all available payloads and diagnostics.
Cleanup updates the JSON sidecar and optional JSON result to reflect retained files;
TIFF-embedded metadata remains the original acquisition record. If cleanup
cannot finish, the CLI reports the remaining files without discarding the images.

Gamma defaults to `device-default` for scans, previews and library settings.
It selects the scanner's built-in tone curve and sends no LUT uploads. Use
`--gamma identity-lut` to explicitly upload neutral custom tables instead.
Neither mode establishes measured sensor linearity. Reset preserves uploaded
tables, but the built-in mode does not select them. `--film` records a media label
and never inverts or applies a colour profile.

`--ir` adds an experimental separate IR8 pass; `--ir-only` requests only IR8.
`--ir-gamma` selects its tone table. IR is limited to the primary holder source
and rejected for `--film mono`. No IR16, dust cleaning, arbitrary focus, sensor
exposure, motorized film transport, eject, or multisampling controls are
advertised. VueScan's published Epson row-averaging method is documented as a
future implementation lead in [the research notes](docs/sources.md).

The CLI rejects conflicting or unused options (for example visible-image mode or depth with
`--ir-only`). Library calls preflight every planned pass before capture. Use
`--io-timeout` for response/block reads, `--scan-timeout` for image acquisition,
and `--settle-seconds` for the delay between passes. That delay is not a readiness
guarantee. Ctrl+C requests cancellation at the next safe transfer boundary.

Interactive scans retain the original fork's progress bars: transferred bytes,
throughput and ETA, with spinners during preparation, settling and saving.
Progress and log messages share stderr without overwriting each other. Bars
are hidden when stderr is redirected. Like the original fork, scans finish
with a short status message giving dimensions, DPI and written filenames.
Use `--json` on `scan` or `preview` to print the complete result to stdout for
scripts; status messages stay on stderr. `--log` and `RUST_LOG` control status
verbosity. The JSON sidecar is saved in either mode.

One frame or explicit area keeps the single-result JSON shape. Multiple explicit
areas produce `{"areas":[{"area":1,"result":{...}},...]}`; multiple holder frames
produce `{"frames":[{"frame":1,"result":{...}},...]}`. Entries follow the requested
selection order, and the combined JSON is printed only after the whole batch succeeds.

`--backend auto` prefers the installed Epson `usbscan.sys` driver on Windows;
`--backend nusb` explicitly selects nusb. Windows nusb acquisition requires a
compatible WinUSB binding. The program does not change drivers. Linux/macOS
use nusb and need the appropriate device permissions. See [Windows notes](docs/windows.md).

## Rust library

The library does not require the `cli` or `python` features.

```rust,no_run
use epscan::{Backend, ScanOptions, ScanSettings, Session};
use std::{path::Path, sync::atomic::AtomicBool, time::Duration};

fn main() -> epscan::Result<()> {
    let mut scanner = Session::connect(None, Backend::Auto, Duration::from_secs(60))?;
    let settings = ScanSettings { rect_mm: [10., 30., 40., 80.], ..Default::default() };
    let options = ScanOptions::default();
    let plan = options.plan(&settings, &scanner.capabilities)?;
    println!("{} passes", plan.passes.len());
    let result = scanner.scan(&settings, &options, Path::new("captures/film"),
        &AtomicBool::new(false), &mut |p| { println!("{} {}/{}", p.phase, p.done, p.total); true })?;
    println!("{}", result.manifest.display());
    scanner.close();
    Ok(())
}
```

Library scans retain raw payloads and protocol traces; automatic cleanup is a
CLI policy. Acquisition streams bounded blocks to numbered files: `<basename>_<n>.bin`,
`.tiff`, `.json`, and `.protocol.jsonl`; IR and thumbnails add `_IR` and
`_thumbnail`. Existing captures are not overwritten. Incomplete transfers remain
`.partial.bin`; completed RGB is retained if a later IR pass fails. Results
contain paths and metadata. `ImageResult::save_tiff` can export a raw-only result
later. TIFF strips preserve sample values and little-endian raw data unless
banding correction is explicitly enabled; large
outputs select BigTIFF. Registration, inversion, normalization, and colour
rendering are not applied. Errors during capture close the session.

For Python 3.13+, build with `maturin develop --release`; see [PYTHON.md](PYTHON.md).
The new Epson API intentionally replaces the cloned Nikon API.

## Adding scanners

Edit [src/capabilities.rs](src/capabilities.rs) for model IDs, identity aliases,
command levels, supported sources/depths, optical ratings, source/focus requests,
transfer policy, holder frame layouts, and continuous strip groups. Discovery, validation, diagnostics, and scan planning use
that registry. Device-reported areas and limits further restrict each profile.
Unknown models are rejected. Add captured identity and protocol fixtures before
adding a model; a new protocol family may also need protocol/session support.
See [the extension guide](docs/adding-scanners.md).

## Validation and provenance

```sh
cargo test --locked --all-targets --features cli
cargo test --locked --doc
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

Hardware testing on the GT-X980/B8 with Windows `usbscan.sys` reproduced a
cold-start rejection: the first acquisition request returns `0x92` and triggers
the scanner's reported warmup state. The model's recovery policy waits for
confirmed readiness and permits one further start; cold and warm scans passed
with the rebuilt CLI. See the [investigation and test results](docs/hardware-debug-2026-09-25.md)
and [hardware coverage](docs/hardware-validation.md). Protocol research and
acknowledgements are recorded in [the sources](docs/sources.md) and [NOTICE](NOTICE.md).

## Binary releases

Download CLI archives from [GitHub Releases](https://github.com/grahamsz/epscan/releases).
Windows x64, Linux x64 (Ubuntu 22.04 or newer), and macOS Intel/Apple Silicon
archives include the executable and license notices. macOS builds are unsigned.
Checksums are supplied in `SHA256SUMS.txt`.

Maintainers: update the package version and `.github/release-notes.md`, commit,
and push a matching `vX.Y.Z` tag. The binary workflow runs CI and publishes the
release only after checks and all four builds pass. Manual runs produce Actions
artifacts; a manual run on a version tag also publishes the release.

License: [MIT](LICENSE-MIT). See [NOTICE](NOTICE.md) for attribution.
