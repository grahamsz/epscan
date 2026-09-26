# Comparing sharpness across holder heights

`--measure-sharpness` computes **Tenengrad** and **variance of Laplacian** for each
main RGB or grayscale image. It works with `scan` and `preview`, one or more explicit
rectangles, and single or multiple holder frames. An additional `--thumbnail`
or infrared pass is not scored; `--ir-only` cannot be combined with
`--measure-sharpness`.

For single-pass RGB or grayscale holder jobs, selected frames on the same V800 strip
(1–6, 7–12, or 13–18) share one acquisition of their bounding region. Each frame
is extracted at its planned pixel rectangle, including its individual overage,
before either metric is calculated. Scores therefore describe each frame's
pixels, excluding the intervening holder gaps. Samples are copied without
resizing or adjustment. IR and thumbnail jobs continue to acquire each frame
separately; explicit rectangles also remain independent.

Each frame retains its own TIFF and JSON scores. Derived-frame metadata links
to the shared strip JSON and records its crop coordinates. The strip JSON
remains after successful cleanup; its raw payload and acquisition trace are
retained with `--keep-intermediates` or if the batch fails.

Use the scores to compare the same film image at different manually set holder
heights. Identify each physical setting in its output basename, such as
`captures/position-4`.

## A repeatable comparison

1. Load the film and choose areas containing useful detail. Keep the same frames
   or explicit rectangles throughout the comparison. Avoid the holder mask and
   frame borders: their strong edges can dominate a score. Negative overage
   crops each nominal frame inward while preserving its center.
2. Set the holder height manually, then acquire the selected regions. Use a
   distinct basename for each setting. For example, after moving the holder to
   position 4:

   ```sh
   epscan scan --holder v800-35mm --frame "1,8,18" --overage -20 --dpi 3200 --depth 16 --gamma device-default --measure-sharpness --basename captures/position-4
   ```

3. Repeat at the other heights with the same frame selection, region, overage,
   DPI, depth, color mode, gamma and film placement. Compare each frame's two scores across
   heights, then inspect the corresponding images. Comparing different frames
   chiefly compares their different content and contrast.

For smaller detail regions, supply explicit rectangles instead of holder
selection. Each rectangle is `x,y,width,height` in millimetres relative to the
required `--source`; semicolons separate areas. For example:

```sh
epscan scan --source film-holder --rect "10,30,10,10;70,70,10,10" --dpi 3200 --depth 16 --gamma device-default --measure-sharpness --basename captures/position-4-details
```

Keep the quotes: PowerShell otherwise treats the semicolon as a command
separator. `--rect "10,30,40,80"` selects one area; four separate numeric arguments
are no longer accepted. Do not combine explicit rectangles with `--holder`,
`--frame`, or `--overage`. Both `scan` and `preview` validate all areas and passes
before capture, then scan sequentially in the supplied order. Repeated identical
rectangles are preserved. Compare the same area's scores across holder positions.

Each area receives its own scores, TIFF and sidecar. A single area retains
`<basename>_1.tiff` naming; multiple areas use `<basename>_area01_1.tiff`,
`<basename>_area02_1.tiff`, and so on. With `--json`, a single area retains the
single-result shape; multiple areas return
`{"areas":[{"area":1,"result":{...}},...]}` after the whole batch succeeds.
Holder batches continue to use the `frames` array.

`--overage -20` reduces a nominal 24 x 36 mm frame to 19.2 x 28.8 mm. Values must
be finite and greater than -100%; the resulting region must still meet ordinary
pixel-size and source-area validation. The holder rectangles remain approximate,
so check that the chosen crops contain image detail rather than mask edges.
A low-resolution preview can help choose the region; compare final scores at
one consistent acquisition resolution.

`--mode gray` acquires one channel directly from the scanner and scores those
samples. Keep the mode fixed across comparisons: grayscale scores need not
match RGB scores, which use a weighted combination of three channels.

Both scores appear in the human INFO output as `measured sharpness`, with 12
fixed decimal places and the annotation `(higher is better)`. The job manifest
records `measure_sharpness: true`. Each image's `metadata.sharpness` object
stores the scores under `tenengrad` and `variance_of_laplacian`.
JSON numeric values retain their full precision; only
the human display is rounded. The JSON sidecar and optional `--json`
result retain that metadata; TIFF exports also embed it. With `--raw-only`, the
scores remain in the JSON metadata. Normal CLI cleanup can remove raw payloads
without removing the recorded scores.

## Metric convention, version 1

These metrics use an original implementation with the following explicit
conventions. Other tools may use different normalization, kernels or thresholds,
so their numerical values need not match.

- Normalize each sample to 0..1 by dividing by 255 for 8-bit data or 65535
  for 16-bit data. Form luma as `0.2126 R + 0.7152 G + 0.0722 B` using those
  encoded sample values for RGB; use the normalized sample directly for
  grayscale (`luminance: "gray"`). No gamma linearization is applied.
- Compute gradients with the unnormalized 3 x 3 Sobel kernels below. Tenengrad
  is the mean of `Gx² + Gy²` over the evaluated pixels. There is no gradient
  threshold and no square root.
- Compute the four-neighbor Laplacian as
  `L = left + right + above + below - 4 × center`. Its score is the population
  variance: the mean of `(L - mean(L))²`, with division by the pixel count
  rather than the count minus one.
- Evaluate both metrics on the acquired pixel grid, excluding the outermost
  one-pixel border. No resizing is performed. The image must be at least 3 x 3
  pixels after scanner width alignment.

The metadata also records `method_version: 1`, normalization and luma labels,
and `evaluated_pixels`, the number of interior pixels used for both scores.

```text
Gx             Gy
-1  0  1       -1 -2 -1
-2  0  2        0  0  0
-1  0  1        1  2  1
```

Higher scores reflect stronger local pixel variation. Noise, film grain and
contrast can increase them as well as sharper image detail. Gamma, clipping,
resolution and scene content also affect the scores. They are comparative
measurements, not absolute optical resolution or a calibrated determination of
the best holder height. Agreement between both scores and visible detail across
repeated scans is more useful than an isolated maximum.
