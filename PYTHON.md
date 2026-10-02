# Python interface

Build from this checkout with Python 3.13+ and Rust:

```sh
python -m pip install maturin
maturin develop --release
```

The extension is `epscan`. No NumPy or proprietary scanner API is required.
Acquisition streams to disk; results are dictionaries containing file paths,
image dimensions/depth, and metadata. This is the Epson API, not compatibility
with the cloned Nikon binding.

```python
import epscan

print(epscan.list_devices())
with epscan.Session() as scanner:
    print(scanner.capabilities())
    result = scanner.scan(
        "captures/film",
        settings={"source": "transparency", "dpi": 300, "depth": 16,
                  "rect_mm": [10, 30, 40, 80], "gamma": "device-default"},
        options={"film": "negative", "export_tiff": True},
        progress=lambda phase, index, done, total: True,
    )
    print(result["rgb"]["tiff"])
```

`Session(device=None, backend_name="auto", timeout=60.0)` selects the sole
supported scanner, or the exact device location reported by `list_devices`.
Available backends are `auto`, `nusb`, and Windows `usbscan`. `timeout` is the
I/O timeout in seconds, greater than zero and at most 214. `capabilities()` returns identity fields;
`diagnostics()` also queries device status and model/source profiles.

All `settings` entries are optional; omitted values use `ScanSettings` defaults:

| Key | Values / default |
| --- | --- |
| `source` | `transparency` (default), `transparency-8x10`, `flatbed` |
| `mode` | `rgb` (default) or `gray` for single-channel grayscale acquisition |
| `dpi` | Integer, default 300; checked against connected device/model |
| `y_oversampling` | Integer 1 through 16, default 1; average extra carriage-axis rows into square output pixels, subject to the source's hardware Y limit |
| `depth` | 8 or 16 bits/channel, default 16 |
| `rect_mm` | x, y, width, height; default `[0, 0, 10, 10]` |
| `gamma` | `device-default` (default, no LUT upload), `identity-lut` (opt-in) |
| `preview` | Boolean, default false; choose a lower DPI and depth 8 for a preview |

Optional `options` entries:

| Key | Default / meaning |
| --- | --- |
| `infrared` | false; add experimental IR8 after RGB |
| `infrared_only` | false; acquire only IR8 |
| `ir_depth` | 8; other depths are rejected by the current model |
| `ir_gamma` | `device-default` |
| `thumbnail` | false; add model-preset RGB preview before final RGB |
| `export_tiff` | true; false keeps raw payload and metadata only |
| `banding` | `None`; `{}` enables the default backlight band reduction preset for visible TIFFs |
| `film` | `negative`; media label, no inversion/profile processing |
| `pass_timeout` | 3600 seconds (one hour) per pass; individual response/block reads still use the session timeout (60 seconds by default) |
| `settle_seconds` | 10 seconds; delay between passes |

Unknown keys and invalid values are rejected. Source eligibility, geometry,
depth and every requested pass are validated before scan configuration. IR-only
uses `ir_depth` and `ir_gamma`; the RGB settings are unused for that pass. Media
labels do not establish the film's physical compatibility with infrared.

For B&W negatives, use `settings={"mode": "gray"}` and
`options={"film": "mono"}` alongside your scan geometry. Grayscale captures
are returned in `result["gray"]`, with one channel at the requested depth;
color captures use `result["rgb"]`. The film label alone does not change the
acquisition mode. Region batching supports both modes.

## Extra carriage-axis sampling

Set `settings={"dpi": 3200, "y_oversampling": 3, ...}` to request 3200 DPI
across the sensor and 9600 DPI along carriage travel. Each consecutive group
of three rows is averaged before writing the packed payload. The output has
the same dimensions and square 3200 DPI pixels as an ordinary 3200 DPI scan;
this increases sampling without enlarging the imported image. It is not
multiple exposures at exactly the same position, and noise improvement has
not been measured.

The connected source's `diagnostics()["model_profile"]["sources"]` entry has
`max_y_dpi`. For the V800/V850 this is Epson's 9600 DPI hardware carriage limit,
distinct from its advertised 12800 DPI interpolated output maximum. Factors
above 1 must satisfy `dpi * y_oversampling <= min(max_y_dpi, source.max_dpi,
capabilities.max_dpi)`; invalid combinations are rejected before configuration.
For example, 3200 DPI permits up to 3x, 4800 DPI up to 2x, and 6400 DPI has no
extra integer factor. A factor of 1 preserves existing output-DPI support.

Averaging uses integer sums and one final round-half-up quantization, preserving
channel separation and 8/16-bit depth. It uses bounded row buffers during USB
ingest. Streaming region callbacks still receive a frame as soon as all of its
averaged rows are committed. Band correction analyzes the completed averaged
strip, and its optional raw TIFF retains those averaged, uncorrected samples.
Individual oversampled rows are not retained. Infrared passes use the same
factor; automatic thumbnails use 1x. Leave previews at 1x for quick positioning.
Metadata records `sampling`, including acquisition/output DPI, acquired pixel
geometry, byte count and averaging method; ESC/I sent/readback parameters remain
available for verification.

## Registered film holders

`scanner.diagnostics()["model_profile"]["holders"]` lists the layouts available
for the connected scanner, including their `holder` identifier, `name`,
`source`, default `frames_mm` and `strip_groups`, `default_format`, `formats`,
and `strip_rects_mm`. Each format entry contains `format`, `name`, `frames_mm`,
and `strip_groups`. Physical `strip_rects_mm` retain each entire opening across
format changes; use those bounds, with adjustment margins, when constructing
holder previews. Fixed sheet holders have no strip rectangles or format choices.
The Epson V800/V850 profile includes `v800-35mm` (35 mm Film Strip Holder,
eighteen full frames or thirty-six half frames), `v800-4x5` (4 x 5 inch Film
Holder, one sheet), and `v800-medium-format` (Medium Format Film Holder,
6x4.5 through 6x17 presets, default 6x6). Use this registry when presenting holder
choices, as NegPy's Epson adapter does, so dimensions stay consistent with the CLI.

```python
with epscan.Session() as scanner:
    holders = scanner.diagnostics()["model_profile"]["holders"]
    holder = next(item for item in holders if item["holder"] == "v800-4x5")
    results = scanner.scan_regions(
        "captures/sheet", holder["frames_mm"],
        settings={"source": holder["source"], "dpi": 1200, "depth": 16},
        options={"film": "negative"},
    )
```

The sheet holder's measured opening is approximately 94 × 119 mm, with a mask
covering part of the nominal film sheet. Its single frame is independent of
strip grouping. Applications can let users adjust these starting rectangles
against a preview before passing them to `scan_regions`; see the
[holder measurements](docs/holders.md).

To choose a medium-format preset before editing or scanning its rectangles:

```python
holder = next(item for item in holders if item["holder"] == "v800-medium-format")
preset = next(item for item in holder["formats"] if item["format"] == "6x7")
regions_mm = preset["frames_mm"]  # Two nominal 56 x 69 mm frames, one strip.
```

These are camera-dependent starting crops, not detected exposures. The medium
holder was measured empty. Its physical opening stays available in the preview
even for a one-frame panorama preset. Rust callers use
`HolderLayout::for_format` or `ScannerModel::holder_frame_with_format`, and
`HolderSelection::frame_format` records and validates the selected preset.

## Backlight band reduction

Set `options={"banding": {}}` to use epscan's experimental negative-film
band reduction, or pass `None`/omit the key to disable it. The same option works
with `scan`, `plan_regions`, and `scan_regions`. It requires `export_tiff=True`
(the default) and a visible RGB or grayscale pass; raw-only and infrared-only
jobs are rejected before scanner configuration.

```python
result = scanner.scan(
    "captures/film", settings=settings,
    options={"film": "negative", "banding": {}},
)
# Read this TIFF to obtain the corrected visible samples.
image = result["rgb"] or result["gray"]
print(image["tiff"], image["metadata"]["banding"]["status"])
```

The correction runs on uninverted visible samples before TIFF export. The
`payload` remains the original packed acquisition, and infrared and thumbnail
images are unchanged. Banding-enabled region batches wait for the complete
strip, fit it once, then export each frame using that shared correction field.
An explicit `detection_roi` preserves frame-local fitting after acquisition.
`region_done` receives the finished TIFF and banding metadata.
Read the returned `tiff` path, rather than
`payload`, when importing corrected images. The metadata records whether a
correction was applied or skipped because no reliable band pattern was found.

The nested dictionary accepts these optional overrides, using the same options
and validation as the Rust API:

| Key | Default / meaning |
| --- | --- |
| `strength` | `1.0`; fraction of the residual-refined sine correction (0 to 1) |
| `dark_full`, `dark_off` | `0.1`, `0.6`; full-strength and cutoff brightness in original samples/full scale |
| `max_frequencies` | `3`; number of candidate frequencies, from 1 to 8 |
| `min_period`, `max_period` | `8.0`, `None`; horizontal band periods in pixels; automatic maximum uses detection width / 5 |
| `window_cycles` | `4.0`; fitting window size, at least 3 cycles |
| `grid_y` | `12`; fitting grid rows, from 2 to 128 |
| `detection_roi` | `None`; optional half-open `[x0, x1, y0, y1]` pixel bounds used for detection |
| `save_raw` | `False`; also save the unchanged visible TIFF with a `_raw` suffix |
| `save_signal` | `False`; also save the fitted signal preview with a `_banding` suffix |

Unknown nested keys and invalid settings are rejected. ROI coordinates are
relative to each output frame. See [banding correction](docs/banding-correction.md)
for the algorithm and limitations.

## Region batches

`scan_regions(basename, regions_mm, *, settings=None, options=None,
max_gap_mm=10.0, progress=None, region_done=None)` accepts a list of physical
`[x, y, width, height]` rectangles in millimetres. Each region overrides
`settings["rect_mm"]`; the other settings and options apply to every region.
The complete list and every resulting acquisition are validated before scanner
configuration or output files are created.

Nearby regions in the same vertical strip are scanned using their combined
bounding box, then extracted into separate images. An ordinary Epson
V800/V850 35 mm Film Strip Holder with 18 selected frames therefore takes three
visible acquisitions, one for each strip. The rectangles supplied by the caller
determine both the bounds and the extracted crops, including any edits to frame
size, position or spacing. A vertical gap greater than `max_gap_mm` splits a
strip into separate acquisitions. Regions in separate strip columns remain
separate; sparse selections can require more scans. Infrared and thumbnail jobs
use separate per-region acquisitions to preserve their existing pass behavior.

Without banding correction, grouped visible scans export each frame as soon as
its final row has arrived in a healthy scanner block. Acquisition runs on a
separate thread, so extraction, TIFF encoding, and application callbacks do not
hold up scanner reads. Pixel data stays in the strip file; notifications and
progress coalesce instead of accumulating image buffers. Completed frames may
therefore arrive before the strip finishes. A later transfer failure keeps and
delivers frames covered by earlier healthy blocks, but never uses a faulted or
partial block to complete a frame. Early-frame provenance records the committed
byte count and an unfinished source acquisition, alongside the frame's own hash.

`plan_regions(regions_mm, *, settings=None, options=None, max_gap_mm=10.0)`
validates and returns this plan using the session's cached capabilities, with
no scanner I/O and no output files. Its `batches` list contains each batch's
zero-based `region_indices`, bounding box in `settings["rect_mm"]`, and
`plan["passes"]`. The sum of these pass-list lengths is the number of hardware
acquisitions. `region_plans` describes the individual requested outputs.

```python
with epscan.Session() as scanner:
    regions = [
        [2.3, 16.5, 24.0, 36.0],
        [2.3, 54.5, 24.0, 36.0],
        [62.1, 16.5, 24.0, 36.0],
    ]
    settings = {"source": "transparency", "dpi": 2400, "depth": 16}
    plan = scanner.plan_regions(regions, settings=settings)
    print([batch["settings"]["rect_mm"] for batch in plan["batches"]])
    # The first two frames share one strip acquisition; the third is separate.
    results = scanner.scan_regions(
        "captures/roll", regions, settings=settings,
        region_done=lambda index, result: print(index, result["rgb"]["tiff"]) is None,
    )
```

Results are standard scan-result dictionaries in the original region-list
order. `region_done(index, result)` runs after each completed output, using the
zero-based input index. Completion callbacks follow frame readiness within each
acquisition, which may differ from input order. Both callbacks run serially on
the calling thread, including during threaded acquisition.
Both `region_done` and `progress` must return
`True` to continue; returning `False` cancels. Callback exceptions are preserved
and re-raised. Region-batch progress receives `(phase, batch_index, done, total)`,
where `batch_index` is zero-based and `done`/`total` track acquired bytes across
all batches. Extraction and saving hold the acquired fraction; 100% is reported
only after all output callbacks succeed. The `regions` phase marks batch completion.
Callbacks may be skipped between acquisition updates; counters remain monotonic.
Hardware I/O and image extraction release the Python interpreter. Completed output files remain available if a later
region fails or the operation is cancelled.

The `scan` progress callback receives `(phase, pass_index, done, total)` and must return
`True` to continue or `False` to cancel. Byte counts apply to transfer phases;
setup/settling phases use progress sentinels. Exceptions from the callback are
re-raised after the Rust layer closes the failed session and preserves files.
The interpreter is released during I/O. Another thread can call
`request_cancel()`; other simultaneous operations on that session raise
`DeviceBusy`. Cancellation applies to an active scan; a new scan clears a prior
cancellation request. Python signals are checked in progress callbacks, so a
warmup wait can delay Ctrl+C until the next progress update. Another thread's
`request_cancel()` is also checked during warmup.

`close()` is idempotent; context-manager exit closes the connection. Capture
errors close it too. Completed passes remain on disk; inspect the JSON manifest
when a later pass fails. Reopening may not recover a desynchronized device.

Exceptions: `ScannerError`, `DeviceNotFound`, `DeviceBusy`, `UnsupportedError`,
and `ScanCancelled`; invalid arguments raise `ValueError`. See [README](README.md)
for hardware status and output semantics. The hand-maintained `epscan.pyi` describes
this API; Rust tests exercise dictionary parsing and simulated Python acquisition.

The registry also includes `v800-slides`, the Epson V800/V850 35 mm Slide Holder:
12 independent 36 x 36 mm starter crops in row-major order, three columns and
four rows. Its `default_film_type` is `positive`; other holders leave that field
null. Python clients can use this preference when choosing film interpretation;
`Session.scan` itself continues to honor the caller's explicit options. Square
crops accommodate portrait and landscape slides and can be tightened individually.
`model_profile.support_status` distinguishes tested and provisional profiles.
V700/V750 is provisional; V500/V550/V600 are discovery-only and fail session
opening with an explanation of the required interpreter.
