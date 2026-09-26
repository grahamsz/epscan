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
| `film` | `negative`; media label, no inversion/profile processing |
| `pass_timeout` | 600 seconds; image transfer deadline |
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
zero-based input index. Completion callbacks follow acquisition order, which
may differ from input order. Both `region_done` and `progress` must return
`True` to continue; returning `False` cancels. Callback exceptions are preserved
and re-raised. Region-batch progress receives `(phase, batch_index, done, total)`,
where `batch_index` is zero-based and `done`/`total` track acquired bytes across
all batches. Extraction and saving hold the acquired fraction; 100% is reported
only after all output callbacks succeed. The `regions` phase marks batch completion. Hardware I/O and
image extraction release the Python interpreter. Completed output files remain available if a later
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
