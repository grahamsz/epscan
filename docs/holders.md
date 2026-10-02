# Film holder frame selection

`v800-35mm` is the Epson V800/V850 35 mm Film Strip Holder supplied with the
scanner, with three strips of six frames. Epson describes loading it in the
[V800/V850 user guide](https://files.support.epson.com/docid/cpd4/cpd41530/source/scanners/source/placing_originals/tasks/placing_filmstrips_pv800_v850.html).
`v800-4x5` is the Epson V800/V850 4 x 5 inch Film Holder, with one sheet position.
See [Epson's sheet-film placement guide](https://files.support.epson.com/docid/cpd4/cpd41530/source/scanners/source/placing_originals/tasks/placing_45_film_pv800_v850.html).

Both `scan` and `preview` accept a holder and 1-based frame numbers or ranges
instead of `--rect`. The holder supplies the source, so `--source` can be omitted:

```sh
epscan preview --holder v800-35mm --frame 1 --overage 5 --basename captures/check
epscan scan --holder v800-35mm --frame "1-5,8-12" --overage 5 --dpi 3200 --basename captures/film
epscan preview --holder v800-4x5 --frame 1 --basename captures/sheet-preview
epscan scan --holder v800-4x5 --frame 1 --dpi 1200 --basename captures/sheet
```

`--holder` and `--frame` require each other. They cannot be combined with
`--rect`; `--overage` also cannot accompany `--rect`. Explicit
`--source film-holder` is allowed with either holder, but a conflicting source is rejected.
Frame numbers are positions in the holder, not exposure numbers printed on film.

Selections combine individual numbers and inclusive ascending ranges separated
by commas. For example, `1-5,8-12` selects ten frames; `3,1-3,8` produces results for
frames 3, 1, 2, and 8 in that order. Duplicates are included once. Zero, reversed ranges,
and frame numbers outside the registered layout are rejected. The entire
selection and every pass are validated before any scan or output file is started.

Each holder frame gets its own image and sidecar. With `--basename captures/film`,
frame 1 produces `film_frame01_1.tiff` and `film_frame01_1.json`; the final number
advances to avoid replacing an existing capture. This naming also applies to a
single selected holder frame. A single explicit `--rect` keeps the original
`film_1.tiff` naming; multiple explicit areas use `film_area01_1.tiff`, and so on.
The batch stops on its first failure or cancellation and retains available raw
payloads and acquisition traces, including completed frames and shared strips. Default
intermediate cleanup runs only after the entire batch succeeds;
`--keep-intermediates` retains them on success too.

Completion summaries remain the default. `--json` keeps the existing result
shape for one frame; multiple frames produce one object containing
`{"frames":[{"frame":1,"result":{...}},...]}` in the requested order. JSON is
printed only after full success. A failed batch reports an error and a nonzero
exit code; individual capture sidecars retain its recorded results.

Single-pass RGB or grayscale scans and previews automatically combine multiple selected
frames from the same registered continuous strip into one acquisition. For the
35 mm holder, strip 1 contains frames 1–6, strip 2 frames 7–12, and strip 3 frames
13–18. The scanner captures only the bounding region needed for that strip's
selected frames, including the gaps between them. For example, selecting `1,3`
also acquires the intervening region, but produces only frame 1 and frame 3
outputs. Different strips are acquired in the order first encountered in the
selection; frame summaries and combined JSON retain the requested frame order.

Each frame's overage is applied before grouping. Extraction copies its exact
planned pixel rectangle from the shared raw payload, preserving sample values,
depth, and scanner alignment without resizing. `--measure-sharpness` scores
each extracted frame independently; holder gaps do not contribute to that
frame's score. Every selected frame still receives a separate TIFF and JSON
sidecar, or a raw payload and JSON with `--raw-only`.

A shared acquisition also keeps a provenance sidecar such as
`film_strip01_1.json`. Each derived frame records its source capture and crop
coordinates. After all requested acquisitions, extractions, exports, and frame
cleanup succeed, the shared raw payload and protocol trace are removed and the
source sidecar records their removal. `--keep-intermediates` retains those files;
an acquisition or extraction failure retains them for diagnosis. A strip with
only one selected frame uses the usual direct frame capture. Jobs with IR or
thumbnail passes remain separate per frame.

The intended workflow is a fixed rectangle centered on each expected frame,
with `--overage` capturing extra surrounding film for a later crop.
Epscan does not detect exposure edges or adjust the layout to each loaded strip.
For example, use `--overage 10 --gamma device-default` to add a centered margin
without uploading a custom tone table. The 2026-09-25 loaded-film preview showed
all eighteen images, with some variation in placement between strips. The layout
does not adapt to that variation.

Overage is the signed percentage change to each **total dimension**, centered on the
nominal frame. Thus 5% expands a 24 x 36 mm frame to 25.2 x 37.8 mm, adding
0.6 mm on each horizontal edge and 0.9 mm on each vertical edge. Negative values
crop instead: `--overage -10` reduces that frame to 21.6 x 32.4 mm, removing
1.2 mm from each horizontal edge and 1.8 mm from each vertical edge. The frame
center stays fixed in both cases. Overage defaults to zero and must be finite
and greater than -100%; -100% and smaller values are rejected. The resulting
dimensions must remain positive and large enough for the normal pixel alignment.
A region outside the scanner source is rejected rather than clipped. Large
positive overage can include
the holder mask or neighboring frames. The scanner's usual pixel alignment still
applies; requested and effective rectangles are recorded in the metadata.

For placement adjustments, both commands also accept one or more explicit
rectangles with a required source, for example:

```sh
epscan scan --source film-holder --rect "10,30,10,10;70,70,10,10" --dpi 3200 --measure-sharpness --basename captures/details
```

Each area is `x,y,width,height` in millimetres relative to that source; separate
areas with semicolons inside the quoted argument. Quotes keep PowerShell from
treating the semicolon as a command separator. A single area uses the same
syntax, such as `--rect "10,30,40,80"`; the old four-separate-value syntax is no
longer accepted. Explicit rectangles cannot be mixed with holder, frame or
overage selection.

All explicit areas and passes are validated before capture. Nearby vertical
regions share one bounding-box scan when they overlap horizontally by at least
half the narrower width and their vertical gap is at most 10 mm. Larger gaps and
separate columns remain separate scans. Outputs retain the supplied order,
including repeated identical rectangles, with the same failure and cleanup policy
as holder batches. `--measure-sharpness` scores each area separately.
Single-area JSON keeps the existing result shape; multiple
areas use `{"areas":[{"area":1,"result":{...}},...]}`, while holder batches keep
the `frames` shape described above.

## V800-family 35mm holder

![35mm holder numbering in device-order coordinates](35mm-holder.svg)

The layout is numbered top to bottom down each strip, then right to left across
the **unrotated, unmirrored device-order preview**: 1–6, 7–12, 13–18. Align the
holder arrows with the scanner arrows as shown in [Epson's placement guide](https://files.support.epson.com/docid/cpd4/cpd41530/source/scanners/source/placing_originals/tasks/placing_filmstrips_pv800_v850.html).

A 100-DPI preview of the user's empty holder on the GT-X980/B8 was acquired on
2026-09-25 with the film-holder source, rectangle 0,0,149.86,246.38 mm. The output
was 584 x 970 pixels (width aligned to eight pixels). The three clear openings
were approximately 24.5 x 228.5 mm. Their measured centers set the horizontal
positions below. The initial layout centered six nominal 24 x 36 mm frames on a
38 mm pitch vertically in each opening, with the first row at y=18.5 mm.

The current layout starts at approximately y=16.5 mm and retains the 38 mm pitch.
All eighteen rectangles are shifted 2 mm toward the source origin to align with
the currently loaded film, following the 2026-09-25 live 100- and 150-DPI preview
comparison in `captures/negpy-holder-debug-20260925`. Horizontal positions and
frame dimensions are unchanged. Coordinates are rounded to 0.1 mm; this alignment
does not establish exposure registration for every loaded strip.

These are **approximate holder positions**, not detected film-image boundaries.
The empty-holder measurement cannot establish where a particular cut strip's
first exposure begins. Use padding and downstream cropping to accommodate strip
placement; `--rect` remains available for individual placement adjustments.
Mounted-slide and medium-format registrations are described below.

All values below are **x, y, width, height in millimetres**, relative to the
film-holder source origin:

| Frame | x | y | Width | Height |
| --- | ---: | ---: | ---: | ---: |
| 1 | 121.5 | 16.5 | 24 | 36 |
| 2 | 121.5 | 54.5 | 24 | 36 |
| 3 | 121.5 | 92.5 | 24 | 36 |
| 4 | 121.5 | 130.5 | 24 | 36 |
| 5 | 121.5 | 168.5 | 24 | 36 |
| 6 | 121.5 | 206.5 | 24 | 36 |
| 7 | 62.1 | 16.5 | 24 | 36 |
| 8 | 62.1 | 54.5 | 24 | 36 |
| 9 | 62.1 | 92.5 | 24 | 36 |
| 10 | 62.1 | 130.5 | 24 | 36 |
| 11 | 62.1 | 168.5 | 24 | 36 |
| 12 | 62.1 | 206.5 | 24 | 36 |
| 13 | 2.3 | 16.5 | 24 | 36 |
| 14 | 2.3 | 54.5 | 24 | 36 |
| 15 | 2.3 | 92.5 | 24 | 36 |
| 16 | 2.3 | 130.5 | 24 | 36 |
| 17 | 2.3 | 168.5 | 24 | 36 |
| 18 | 2.3 | 206.5 | 24 | 36 |

The implementation table is in `src/capabilities.rs`. `epscan dump` includes
registered layouts in `model_profile.holders`. Rust callers can resolve a
rectangle with `ScannerModel::holder_frame(Holder::V800Film35mm, frame, overage)`;
`ScanOptions::holder_selection` records the choice and validates its agreement
with `ScanSettings`. Sidecar and image metadata include the holder, frame,
overage, nominal rectangle, and adjusted rectangle.

The local calibration capture is
`captures/holder-calibration-20260925/full-holder_1.tiff`; captures are ignored by
Git. The diagram shows the registered rectangles; it is not a claim of detected
image content or measured film registration.

Hardware checks on 2026-09-25, using the initial y=18.5 mm layout, completed frame
1 as a 100-DPI preview at 0% overage, frame 8 as a 300-DPI RGB16 scan at 5%, and
frame 18 as a 100-DPI preview at 10%. All recorded the selected holder/frame and
retained TIFF plus sidecar after cleanup.

Repeated full-holder and frame-18 previews from those checks exposed a positioning limitation:
the opening's bottom edge appears at full-preview row 966, but at cropped row
144 plus requested origin 814 = 958. The eight-row difference is about 2.03 mm
at 100 DPI. Regular RGB8 scans after a power cycle, using `--gamma device-default`,
placed the same bottom edge at command-space 243.459 mm (100 DPI) and 243.544 mm
(300 DPI), versus 245.491 mm in the full preview. A separate top-edge crop starting
at y=10 mm aligned within one 100-DPI pixel, so a constant two-millimetre shift
would not fit both ends. Requested and read-back coordinates agree, and TIFF
sample hashes match acquisition hashes. The cause was not established, and these
checks did not yield a blanket offset correction. The current 2 mm layout shift
aligns the rectangles with the loaded film; it is not a correction for that
unresolved acquisition difference. Use padded approximate captures, with precise
cropping performed downstream.

One additional RGB8 test using the identity LUT failed before acquisition when
the blue gamma-table upload returned STX instead of ACK. The subsequent identity
query also failed. After the user power-cycled the scanner, the two regular
RGB8 scans above passed using device-default gamma. This holder work does not
resolve that protocol failure; its trace and empty partial payload were retained.

## Epson V800/V850 4 x 5 inch Film Holder

`v800-4x5` contains a single sheet position, selected with `--frame 1`. It uses
the normal `transparency` / `film-holder` source and has no continuous strip
groups. Align the holder arrows with the scanner arrows as shown in
[Epson's placement guide](https://files.support.epson.com/docid/cpd4/cpd41530/source/scanners/source/placing_originals/tasks/placing_45_film_pv800_v850.html).

A loaded-holder RGB8 preview on the GT-X980/B8 on 2026-09-26 measured the usable
opening at approximately 94 × 119 mm. This is smaller than the nominal 4 × 5 inch
sheet because the holder mask covers its edges. The registered rectangle is in
the unrotated, unmirrored device-order preview, relative to the film-holder
source origin:

| Frame | x (mm) | y (mm) | Width (mm) | Height (mm) |
| --- | ---: | ---: | ---: | ---: |
| 1 | 27.0 | 48.0 | 94.0 | 119.0 |

The measurement used a 300-DPI full-source preview of
`0,0,149.86,246.38` mm, producing 1768 × 2910 pixels. Its local capture is
`captures/holder-4x5-20260926/overview-300_1.tiff`, with `diagnostics.json` and
`result.json` in the same directory; these captures are ignored by Git.
The opening edges were slightly skewed within a millimetre, so these are
approximate holder coordinates, not detected image boundaries. Use `--overage`
to adjust the centered size, or `--rect` for placement adjustments. NegPy's
holder preview allows moving and resizing the crop before scanning.

Hardware verification on the same day completed the registered
`epscan preview --holder v800-4x5 --frame 1` capture and TIFF export at 100 DPI
(368 × 469 RGB8 pixels). NegPy's adapter also completed a 300-DPI bounded
preview of the loaded negative. It acquired approximately
`22.01,43.01,104.31,128.95` mm, including 5 mm of adjustment room around the
opening and the scanner's width alignment. This transfers about 63% fewer
pixels than the full transparency area. The displayed preview retains
1228 × 1523 pixels after removing alignment padding; its crops still use
absolute source coordinates. Local verification records are in
`captures/holder-4x5-20260926/negpy-verification.json` and
`registered-cli-preview_frame01_2.json`.

The layout is included in `epscan dump` under `model_profile.holders` and can
be resolved in Rust with
`ScannerModel::holder_frame(Holder::V800Film4x5, 1, overage)`. Requests for frame
2 or higher are rejected before acquisition.

## Medium-format holder and frame presets

`--holder v800-medium-format` selects the **Epson V800/V850 Medium Format Film
Holder** using the normal `transparency` / `film-holder` source. It has one
continuous opening. Epson lists a maximum medium-format film size of 6 x 20 cm;
follow [Epson's medium-format placement guide](https://files.support.epson.com/docid/cpd4/cpd41530/source/scanners/source/placing_originals/tasks/placing_medium_film_pv800_v850.html)
to align the holder arrows with the scanner arrows.

An empty-holder 300-DPI RGB8 full-source preview on the GT-X980/B8 on 2026-09-26
measured this usable aperture, in device-order source coordinates:

| Strip | x (mm) | y (mm) | Width (mm) | Height (mm) |
| --- | ---: | ---: | ---: | ---: |
| 1 | 45.1 | 33.5 | 57.6 | 200.0 |

The full-source rectangle was `0,0,149.86,246.38` mm, producing 1768 x 2910
pixels. The local capture is
`captures/holder-medium-20260926/overview-300_1.tiff` (ignored by Git).
There was no film available, so this measurement establishes the opening only;
exposure positions and film-loaded registration have not been verified.

The registered `6x4.5` CLI preview was also verified on that empty holder:
four 216 x 163 RGB8 outputs at 100 DPI came from one 216 x 677 strip
acquisition. NegPy's default `6x6` preview was verified at 300 DPI, acquiring
approximately `40.132,28.533,67.733,209.973` mm and delivering one continuous
798 x 2480 strip preview with three frame crops. This bounded preview uses
about 39% of the full transparency area. Local evidence is in
`captures/holder-medium-20260926/registered-645_strip01_1.json` and
`negpy-verification.json`.

`--frame-format` applies nominal exposure crops within that opening:

| Format | Crop across/along strip (mm) | Capacity | Starting y positions (mm) |
| --- | --- | ---: | --- |
| `6x4.5` | 56 x 41.5 | 4 | 47.5, 91, 134.5, 178 |
| `6x6` (default) | 56 x 56 | 3 | 47.5, 105.5, 163.5 |
| `6x7` | 56 x 69 | 2 | 63.5, 134.5 |
| `6x8` | 56 x 76 | 2 | 56.5, 134.5 |
| `6x9` | 56 x 84 | 2 | 48.5, 134.5 |
| `6x12` | 56 x 112 | 1 | 77.5 |
| `6x17` | 56 x 168 | 1 | 49.5 |

All medium-format crops start at x=45.9 mm. Each preset uses the largest count
that fits the aperture with a nominal 2 mm inter-frame gap, centered as a group.
Frame numbers run from top to bottom. These sizes and gaps are editable starting
points: camera gates, advance spacing, and the position of a cut strip vary.
For example, the 6x4.5 dimensions follow the
[Pentax 645NII specification](https://www.ricoh-imaging.co.jp/english/products/filmcamera/medium/645n2/spec.html);
they are not a universal dimension for every 645 camera. The other presets
likewise do not identify the camera that exposed the film.

```powershell
epscan preview --holder v800-medium-format --frame-format 6x6 --frame 1-3
epscan scan --holder v800-medium-format --frame-format 6x9 --frame 1-2 --dpi 2400
```

Selected frames share one bounding-box acquisition for single-pass RGB or
grayscale; outputs are cropped separately. As with other holders, `--overage`
changes centered crop dimensions. For individual custom positions or sizes,
use `--source film-holder --rect "x,y,width,height;..."` instead. A format cannot
be combined with `--rect`, and unsupported holder/format combinations or frame
numbers beyond the selected preset's capacity fail before USB is opened.

In the native diagnostic registry, `default_format` names the initial preset
and `formats` lists each supported format's identifier, ASCII display name,
frame rectangles, and strip groups. The top-level `frames_mm` and `strip_groups`
retain the default layout for existing consumers. `strip_rects_mm` describes
the full aperture independently of exposure format, so NegPy's 300-DPI preview
can retain adjustment room along the entire strip without scanning the full
transparency area. Individual selected CLI previews retain the normal
frame/bounding-box behavior rather than acquiring all unused aperture space.

## 35 mm half-frame preset

The existing 35 mm holder also supports `--frame-format 35mm-half`. Its nominal
24 x 18 mm crops use a 19 mm pitch along each strip. The calibrated x positions
and first y=16.5 mm are unchanged; each strip contains twelve frames rather than
six. Numbering is 1-12 on the right, 13-24 in the middle, and 25-36 on the left.
This is an inferred starter grid based on the existing full-frame calibration;
it has not been verified with loaded half-frame film.

```powershell
epscan scan --holder v800-35mm --frame-format 35mm-half --frame 1-12 --dpi 2400
```

The default remains `35mm`, preserving every existing full-frame coordinate.
NegPy exposes both formats in the same physical-holder workflow, with adjustable
crop dimensions, frame positions, and strip spacing. The physical preview strip
extent spans both starter grids; it does not change when switching formats.

## Epson V800/V850 35 mm Slide Holder

`v800-slides` registers the official twelve-position mounted-slide holder.
Frames are numbered left to right, then top to bottom in the unrotated device
preview: 1-3, 4-6, 7-9, 10-12. Physical viewing direction can reverse left/right.
Each position starts with a 36 x 36 mm crop to accommodate either portrait or
landscape exposures. Tighten crops to the mount apertures in NegPy; these are
holder coordinates, not automatic image-edge detection. Slides are independent
positions, not continuous strips. NegPy displays a 3-by-4 grid with an individual
checkbox and crop per slide. Shift adjusts all crops.

```powershell
epscan preview --holder v800-slides --frame 1-12
epscan scan --holder v800-slides --frame 1-12 --dpi 3200 --basename captures/slides
```

CLI film metadata defaults to `positive` for this holder; explicit `--film`
overrides it. NegPy defaults Film to Slide when switching to this holder and
preserves subsequent manual choices. Square crops include some mount area.

The layout was measured from an empty-holder 300-DPI RGB8 preview on the local
GT-X980/B8 on 2026-09-26. A second full preview with landscape and portrait slides
confirmed both orientations inside the registered squares. CLI previews of the
two occupied positions (3 and 10 in device order) completed successfully. Local
captures are under `captures/holder-slides-20260926` (ignored by Git).
Coordinates are rounded to 0.1 mm and may need adjustment for another holder.
The first-row origins are (2.7,33.1), (56.1,33.1), (109.3,33.1) mm; subsequent
rows start near y=91.1,151.1,209.1 mm. Full coordinates live in
`src/capabilities.rs` and the diagnostic holder registry.
