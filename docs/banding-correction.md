# Periodic band correction: Rust implementation guide

## Current implementation: refined sine backport (method version 2)

epscan owns 'crates/negative-banding', an independent backport of the Photoshop
plugin engine. Each repository builds from its own copy. The only waveform is the accepted pure sine. Detection retains the
original FFT/coherence method; local amplitudes are refined using signed
residuals on disjoint training and held rows, with up to four conservative
updates. Frequencies and phases remain fixed. Unsupported regions retain their
initial amplitudes, so the darkness guard remains enabled.

Defaults are **100% fitted strength**, full darkness weight below **10%**,
fading to zero at **60%**. '--banding-strength 0.8' remains available for a weaker
result. The kernel is now 'clamp(raw * (1-strength*darkness*refined_sine_sum))',
rounded once at the original 8/16-bit depth. It no longer applies an exponential.
The raw payload and optional raw TIFF remain unchanged. Cropped outputs reuse
the full acquisition's fit in source coordinates.

Photoshop uses the same fit but encodes layers/masks with its native 0..32768
sample grid and sequential component clipping. epscan retains full TIFF sample
precision and clips the summed correction once; tiny quantization differences
and differences at clipping limits are expected. Photoshop's 66% initial layer
opacity is already compensated in its mask and is not applied again in epscan.

The optional signal PNG now shows signed intensity removed, normalized by source
full scale, after darkness and strength: red darkens, blue brightens, white is
zero. Its 'range_fraction_full_scale' replaces the old log-gain range.
Metadata records 'method_version: 2', 'residual_refined_linear_sine', the local
engine version, original detector diagnostics, and residual-refinement reports.
Detector metrics still describe the initial exponential baseline; refinement
metrics describe the float fit and are not an independent held-out error estimate.

See [engine API and tests](../crates/negative-banding/README.md).

## Historical implementation notes (method version 1)

The remaining specification records the original Python/exponential method and
its 80% preset for provenance. Its defaults, output kernel, and signal-PNG units
have been superseded by method version 2 above.


This guide describes the experimental correction developed in the sibling
`test_banding` workspace on 2026-09-25. It learns narrow periodic signals shared
across the height of a negative, estimates their amplitude across the image, and
attenuates the correction in brighter/thinner parts of the negative. The
multiplicative method is implemented as an optional Rust processing stage in
[`src/scan/banding`](../src/scan/banding/mod.rs). The additive formulas below
describe the Python reference only; the Rust feature uses multiplicative
correction throughout.

The accepted working recipe is **multiplicative correction, 80% strength, with a
darkness mask that is full below 10% of sample full scale and reaches zero at
60%**. Those values are an empirical preset for these scans, not a universal
scanner calibration. They are the defaults for Rust's `--reduce-banding` flag.
The Python CLI still defaults to additive correction,
50% strength, and a 30% cutoff: supply the accepted settings explicitly.

## Reference code and scope

The implementation being described is `../../test_banding/test3.py`, relative to
this document. The older `test.py` and `test2.py` are earlier approaches; do not
combine their period-search rules with this one. `test3.py` imports TIFF helpers
and `robust_fit` from `test.py`.

| Operation | Python reference |
| --- | --- |
| Read/write integer images; robust weighted least squares | `test.py`: `read_image`, `write_image`, `robust_fit` |
| Separate fitting and validation rows | `test3.py`: `sample_image` |
| Frequency detection and optional detection crop | `strip_profiles`, `spectral_data`, `detect_frequencies`, `detect_with_roi` |
| Estimate amplitude maps | `fit_amplitude_maps` |
| Reconstruct a correction at source coordinates | `prepare_rows`, `interpolate_rows`, `correction_rows` |
| Mask and quantized pixel operation | `source_brightness`, `dark_mask`, `correct_pixels` |
| Measure residual periodicity | `validation_report` |
| Show what is actually applied | `show_correction_masks.py` |

These sibling files are local development references, not dependencies that
should be required by the Rust library. Some large scan fixtures were deleted
to save disk space. Keep small deterministic fixtures and numerical golden
results when implementing the port.

## Coordinate system and signal model

Use source storage coordinates throughout: `(x, y)` means column and row,
with the origin at the top left. A grayscale array is indexed `[y, x]`; RGB is
`[y, x, channel]`. Width and height are not interchangeable.

**Top-to-bottom vertical stripes vary across x.** Average information from
different rows to find a common frequency, but take each one-dimensional FFT
across columns. A correction that oscillates with y instead removes horizontal
stripes. Portrait dimensions alone do not establish which direction the
artifact runs.

For channel `c`, use:

```text
C_c(x, y) = sum over k [ A_c,k(x, y) * cos(2*pi*f_c,k*x + phi_c,k) ]
```

- `f`: cycles per source pixel, not radians/pixel or cycles/image.
- `phi`: radians, referenced to source column zero.
- `A`: nonnegative amplitude, varying smoothly in x and y.
- In multiplicative mode, `C` is a log-gain correction.
- Frequencies and phase are constant within a fitted image; amplitude varies.

The negative must still be in its original, uninverted sample polarity: dense
areas have small values. Do this before negative inversion or display curves.
The model was fitted to stored sample values; no linear-light or optical-density
calibration was established. Do not silently gamma-convert the samples when
trying to reproduce these results.

The Python TIFF loader preserves Orientation metadata without rotating the
array. The tested new scans had Orientation=1. If a future TIFF importer
normalizes rotation, transform the band axis, ROI, phase origin, and output
metadata consistently. A 90-degree display rotation must not silently change
the axis used for analysis.

DPI only annotates physical spacing:

```text
period_pixels = 1 / f
period_mm = period_pixels * 25.4 / dpi
```

Detect in pixels. Do not force a 4.25 or 4.3 mm period from an uncertain TIFF DPI
tag. The two September 2023 files were tagged 5834 and 2654 DPI; 3200 DPI was
subsequently used as a working assumption, without resampling either image.

## Processing stages

1. Validate the integer source and configuration; keep the source unchanged.
2. Sample disjoint fitting and validation rows in original coordinates.
3. Find coherent spectral peaks, optionally using a clear-sky detection ROI.
4. Convert ROI-relative phases back to the full source coordinate system.
5. Fit local amplitudes over the whole source for all selected carriers jointly.
6. Interpolate the amplitude maps and reconstruct the signed correction.
7. Multiply by the original-negative darkness mask and global strength.
8. Correct original integer samples through floating-point intermediates, then
   quantize once into a separate 16-bit output.
9. Report held-out residuals and render small, correctly aligned diagnostics.

If no frequency passes the detection gates, retain the input unchanged and
report that no supported line was found. Do not force a guessed period merely
to produce a visibly different output.

## 1. Sample the original image

Let `M=65535` for unsigned 16-bit samples, or `255` for unsigned 8-bit samples.
Normalize `I = sample / M` using `f64` for the reference implementation.

For source height `H`, choose an even row stride:

```text
s = max(2, 2 * ceil(H / (2 * 768)))
training_y = 0, s, 2*s, ... < H
validation_y = s/2, s/2+s, ... < H
```

This gives at most about 768 rows in each set. Keep their actual y coordinates;
the index within the sampled array is not a source coordinate. Read full-width
rows, and process channels independently for detection and amplitude fitting.

Transform sampled pixels as:

```text
multiplicative: L = ln(max(I, 1/M))
additive:       L = I
```

The positive floor is for logarithmic fitting only. Applying multiplicative
correction to the original samples later must preserve original zero-valued
pixels; additive correction can change zero-valued pixels.

Split each sampled set into up to 16 consecutive row groups, as evenly as
possible. Match NumPy `array_split`: earlier groups receive any extra row.
The profile for detection in each group is:

```text
multiplicative: profile[x] = ln(mean_y(exp(L[y,x])))
additive:       profile[x] = mean_y(L[y,x])
```

The first expression is the log of the arithmetic mean of floored light levels.
It is deliberately different from `mean_y(log(I))`, which is used later for the
local multiplicative amplitude fit. Logging before averaging gave very dark or
clipped pixels too much influence during frequency detection.

## 2. Detect coherent frequencies

Here `N` is the detection width: full image width, or ROI width if a detection
crop is selected. Perform these steps independently for every strip profile.

### Detrend and transform

Fit and subtract an ordinary cubic polynomial using a normalized column
coordinate spanning `[-1,1]`. This removes broad image brightness variation.
Use the symmetric four-term Blackman-Harris window:

```text
t = 2*pi*x/(N-1)
w[x] = 0.35875 - 0.48829*cos(t) + 0.14128*cos(2*t) - 0.01168*cos(3*t)
```

Take the real-input FFT of `residual*w`, zero-padded to **16*N** samples. Use the
forward convention `exp(-i*2*pi*f*x)` and normalize each complex coefficient by
`2/sum(w)`. FFT index `j` has frequency `j/(16*N)` cycles/source pixel.
Zero padding refines peak sampling; it does not supply additional image detail.

For complex coefficients `z_b(j)` over strip index `b`, calculate:

```text
z_mean = mean_b(z_b)
amplitude = abs(z_mean)
coherence = abs(z_mean) / max(mean_b(abs(z_b)), 1e-30)
unit_coherence = abs(mean_b(z_b / max(abs(z_b), 1e-30)))
```

The unit-phase measure prevents a single strong strip from dominating the
agreement test. Record both measures.

### Select peaks

The current defaults and gates are:

| Quantity | Value |
| --- | --- |
| Minimum period | 8 pixels |
| Maximum period | `N/5` by default |
| Allowed custom bounds | `2 <= min_period < max_period <= N/3` |
| Maximum selected components | 3 by default; configuration permits 1 through 8 |
| Minimum coherence | 0.55 |
| Minimum unit coherence | 0.50 |
| Minimum spectral prominence | 4.5 |
| Minimum retained relative amplitude | 5% of the largest previously selected component |
| Minimum frequency separation | `4/N` cycles/pixel |

Find local maxima using `amp[j] > amp[j-1] && amp[j] >= amp[j+1]`, excluding
the DC and final Nyquist bins, within the period range. A candidate
must also exceed `64 * f64_epsilon * max(abs(detection_profiles))`.
Prominence is amplitude divided by `max(median_flank_amplitude, 1e-30)`, with
indices relative to the candidate padded FFT bin:

```text
[-12*16, -4*16) union [+4*16, +12*16)
```

Clip flank indices to valid non-DC bins. This is an amplitude ratio, not power
or a calibrated probability. Guard an empty or degenerate flank set in Rust.

Refine the peak by parabolic interpolation of log amplitude. For adjacent values
`a,b,c = ln(max(amplitude, 1e-30))`:

```text
delta = clamp(0.5*(a-c)/(a-2*b+c), -0.5, +0.5)
f = clamp((j+delta)/(16*N), 1/max_period, 1/min_period)
```

Use zero offset if the denominator is zero. Measure the complex phasor directly
at this refined frequency, rather than interpolating phase between FFT bins:

```text
z_b(f) = 2/sum(w) * sum_x(residual_b[x] * w[x] * exp(-i*2*pi*f*x))
global_amplitude = abs(mean_b(z_b(f)))
phi = arg(mean_b(z_b(f)))
```

Apply the same measurement to held-out rows. Reject the candidate if the phase
difference exceeds `pi/3`, or its held-out amplitude is less than one quarter
of its training amplitude. Sort accepted candidates by
`raw_peak_amplitude * unit_coherence^2`; greedily apply the relative-amplitude
and frequency-separation gates until the component limit is reached.
Do not automatically add harmonics or assume all frequencies are harmonics.
For Python parity, coherence, unit coherence, prominence, and ranking still
refer to the original FFT peak bin; the stored component amplitude and phase
are measured again at the refined frequency.

### Optional detection ROI

Use a clear region when scene edges or clouds dominate the full-width spectrum.
This is a detection aid, not the area to which correction is restricted.

The Python CLI order is **`X0 X1 Y0 Y1`**, with half-open bounds. Prefer named
Rust fields to avoid confusing it with `(x,y,width,height)` used elsewhere.
Select training and held-out rows by their original y coordinates, then crop
columns `[x0,x1)`. Require at least eight rows from each sampled set and enough
width for the configured period search.

After detection, translate the phase to source column zero:

```text
phi_source = wrap_to_pi(phi_roi - 2*pi*f*x0)
```

The minus sign matters: `cos(2*pi*f*(x-x0)+phi_roi)` must equal
`cos(2*pi*f*x+phi_source)`. Do not apply the offset twice. Amplitude fitting
still uses the full image and the same global sampled rows.

## 3. Fit the spatial amplitude maps

Let `P` be the longest detected period. Set the x-window span to:

```text
required = 4*P
if multiple frequencies:
    required = max(required, 2/min_pairwise_frequency_separation)
span = min(image_width, ceil(required))
```

The default is four cycles per fitting window; permit at least three and reject
a final span shorter than `3*P`. Nearby carriers need a larger window to separate
their amplitudes. Fit all selected frequencies simultaneously.

Use full-width windows at the edges, rather than fitting a truncated half-window:

```text
nx = max(2, ceil((image_width-span)/(span/4)) + 1)
x_centers = linspace((span-1)/2, image_width-1-(span-1)/2, nx)
```

Deduplicate x centers; a full-image span gives one distinct center. Use up to
12 y centers uniformly from `0` to `H-1`. At each y center, form a triangularly
weighted mean profile of the **transformed individual samples**:

```text
y_radius = max(1.5*H/(configured_grid_y-1), 2*median(training_row_spacing))
weight_y = max(0, 1 - abs(source_y-center_y)/y_radius)
profile = sum_y(weight_y * L) / sum_y(weight_y)
```

At each x center, take a complete `span`-pixel window. Its first column is
`clamp(round_ties_even(center_x-(span-1)/2), 0, width-span)`.
Fit this design matrix using normalized local coordinate `u` in `[-1,1]`:

```text
D = [1, u, u^2, cos(2*pi*f1*x), sin(2*pi*f1*x), ...]
```

The carrier coordinate `x` is the **absolute source column**, even inside a
local window. The polynomial models local scene brightness; it is not part of
the correction signal.

### Robust weighted least squares

Use a symmetric Hann window across the local span. Start with weighted least
squares, multiplying each design row and target by `sqrt(Hann)`. Then perform
exactly three reweighting iterations:

```text
r = profile - D*coefficients
s = 1.4826 * median(abs(r)) + 1e-12
row_multiplier = sqrt(Hann / (1 + (r/(2*s))^2))
coefficients = least_squares(D * row_multiplier, profile * row_multiplier)
```

Use a stable QR/SVD least-squares solver rather than explicitly inverting normal
equations for the coefficients. For reference parity, `s` above uses an
uncentered absolute residual; it is not the centered MAD used below.

### Keep only amplitude supported by the shared phase

For each carrier, let `p=(cos_coefficient, sin_coefficient)` and:

```text
v = (cos(phi), -sin(phi))
v_perp = (sin(phi), cos(phi))
parallel = dot(p, v)
quadrature = dot(p, v_perp)
```

Projecting onto a single shared phase prevents each tile from freely adopting
the phase of unrelated local scene detail. Negative projected amplitude is
suppressed rather than turned into an inverted correction.

After the final robust solve, recompute `r = profile - D*coefficients` using
the final coefficients. Estimate residual noise and coefficient uncertainty:

```text
noise = 1.4826 * median(abs(r - median(r)))
covariance = pseudoinverse(D^T * diag(Hann) * D)
uncertainty = noise * sqrt(max(0, v^T * covariance_carrier_pair * v))
```

The covariance uses the original Hann weights, not the final robust weights;
this is the prototype's heuristic uncertainty estimate, not a calibrated
confidence interval. Retain the appropriate 2x2 block from the full joint fit.

Then shrink and cap the amplitude:

```text
unwanted_power = quadrature^2/3 + (2*uncertainty)^2
support = clamp(1 - unwanted_power/max(parallel,1e-30)^2, 0, 1)
cap = max(6*global_amplitude, 1e-12)
A = clamp(max(0,parallel)*support, 0, cap)
```

Store diagnostic confidence as `support` when `parallel > 0`, otherwise zero,
and count capped tiles. Support has already been
applied to `A`: **do not multiply it into the correction a second time**.
Likewise, do not apply the global 80% strength until the output stage.

## 4. Interpolate, mask, and apply

Bilinearly interpolate each component's small amplitude grid in source pixel
coordinates. Outside the supported center grid, extend the nearest edge value.
Handle a single x or y center as constant interpolation. Precompute x-interpolated
rows for each grid-y center and the carrier cosines to avoid repeated work.

Sum the interpolated carriers to get `C(x,y)`. Derive the mask from the original
integer pixel, before correcting any channel:

```text
grayscale: brightness = original / M
RGB:       brightness = max(original_R, original_G, original_B) / M
t = clamp((brightness - 0.10) / (0.60 - 0.10), 0, 1)
mask = clamp(1 - t^3*(10 - 15*t + 6*t^2), 0, 1)
applied = strength * mask * C(x,y)
```

The mask is a quintic smoothstep. These thresholds are fractions of encoded
full scale, not percentiles, relative-to-image-maximum brightness, or physical
negative density. With the accepted settings:

| Original brightness | Mask | Effective strength |
| --- | --- | --- |
| At or below 10% | 1 | 80% |
| 35% | 0.5 | 40% |
| At or above 60% | 0 | 0% |

Use one shared mask for RGB, based on the brightest original channel. Keep the
original RGB triplet available while applying all three channel models. Do not
derive later masks from already modified channel values. The prototype does
not smooth the mask or create a semantic cloud/sky mask.

Apply to the original normalized sample `I`, not its logarithmic floor:

```text
multiplicative: corrected = I * exp(-applied)
additive:       corrected = I - applied
output = round_ties_even(clamp(corrected, 0, 1) * M)
```

If `mask == 0`, explicitly copy the source sample unchanged. Preserve the input
bit depth; an 8-bit comparison PNG is only a preview. NumPy `rint` rounds ties
to even; Rust `round()` has different tie behavior. Matching rounding matters
for parity checks even though most pixels do not land exactly on a tie.

An illustrative Rust pixel kernel, after validating finite inputs and options:

```rust
fn dark_weight(brightness: f64, full: f64, off: f64) -> f64 {
    let t = ((brightness - full) / (off - full)).clamp(0.0, 1.0);
    (1.0 - t*t*t * (10.0 - 15.0*t + 6.0*t*t)).clamp(0.0, 1.0)
}

fn apply_mult_u16(original: u16, correction: f64, mask: f64, strength: f64) -> u16 {
    if mask == 0.0 || strength == 0.0 {
        return original;
    }
    let input = f64::from(original) / 65535.0;
    let applied = strength * mask * correction;
    let value = (input * (-applied).exp()).clamp(0.0, 1.0);
    (value * 65535.0).round_ties_even() as u16
}
```

Reject invalid options (`0 <= full < off <= 1`, strength in `[0,1]`, finite
values) before fitting. Check solver outputs for nonfinite values and handle
rank-deficient fits explicitly. Do not silently cast NaNs into image samples.

## Rust integration in this repository

### Preserve the acquisition/export distinction

[`src/session/image.rs`](../src/session/image.rs) defines `ImageResult`, which
points to a packed raw payload on disk. `save_tiff` explicitly exports unchanged
samples. That contract is unchanged. The public
[`scan::banding`](../src/scan/banding/mod.rs) module analyzes the original payload
and renders separate corrected artifacts through an explicit opt-in API:

```rust,ignore
use epscan::scan::{ScanOptions, banding::BandingOptions};

let options = ScanOptions {
    banding: Some(BandingOptions {
        save_raw: true,
        save_signal: true,
        ..Default::default()
    }),
    ..Default::default()
};
```

For an existing packed `ImageResult`, call
`banding::export(&mut image, final_path, &banding_options, &cancel)`, where
`banding_options` is a `BandingOptions`. It analyzes and
exports together, sets `image.tiff` to the corrected TIFF, and attaches a
`banding` metadata object without changing the source payload. The fitted model
is currently private; saved-model reuse is not a public API. Diagnostics record
source dimensions, options, ROI, per-channel frequencies/phases, amplitude and
confidence grids, and held-out validation results. The correction is evaluated
from these small grids without allocating a full-resolution correction image.

Use these existing integration points:

- [`src/scan/frame.rs`](../src/scan/frame.rs): after publishing the completed raw
  payload and acquisition metadata, before optional export and cleanup.
- [`src/scan/crop.rs`](../src/scan/crop.rs): the separate derived-frame export
  path lets `scan_regions` apply the completed strip's fitted correction field
  to each crop using its source-coordinate offset.
- [`src/scan/sharpness.rs`](../src/scan/sharpness.rs): useful patterns for checked
  image sizes, reading rows, bounded allocations, and cancellation.
- [`src/scan/mod.rs`](../src/scan/mod.rs): `ScanOptions::banding` and preflight;
  CLI plumbing lives under `src/bin/epscan`.

There is no need to change the ESC/I wire protocol in `src/session/esci.rs`.
Only visible grayscale/RGB passes are corrected. Infrared and thumbnail passes
retain their existing export behavior; infrared-only and raw-only requests
cannot enable band correction.

The current raw format is row-major, packed, with grayscale or interleaved RGB
samples. Sixteen-bit samples are **little-endian**. For sample `(x,y,c)`:

```text
sample_index = (y*width + x)*channels + c
byte_offset = sample_index * bytes_per_sample
```

Use checked arithmetic for dimensions/offsets and decode with
`u16::from_le_bytes`; avoid unchecked native-endian casts of byte buffers.
Follow the existing exact-payload-length checks.

Current acquisition metadata says samples are not inverted or rotated and that
linear gamma is not verified. `--film` labels the medium; it does not make a
negative positive. Apply this mask to the original negative-polarity samples.
The sharpness module's luminance formula is not the banding mask: use the
maximum original RGB channel as specified above.

With the default detection area, `scan_regions` batches wait for the whole strip,
fit that shared acquisition once, and reuse the model for every frame. A crop
with origin `(ox,oy)` evaluates the field at source coordinates `(x+ox,y+oy)`,
including amplitude-grid interpolation. Its carrier phase in crop coordinates
therefore gains `2*pi*f*ox`, the reverse of converting a detection-ROI phase to
source coordinates. The unchanged packed frame and optional raw TIFF still
contain exact cropped acquisition samples. The optional signal PNG shows the
field applied to that frame at the same source offset.

An explicit Python `detection_roi` retains its final-frame pixel coordinates: each
frame is fitted independently after the shared acquisition completes, and the
same ROI must be valid for every requested frame. Single-region and standalone
exports also fit their own image. The CLI's `--banding-roi` and derived-frame
exports keep their per-frame fitting behavior. Banding-enabled region scans never publish
frames before their acquisition completes; they need the full analysis source.

Banding metadata distinguishes `analysis_scope: "shared_capture"` from
`"output_image"`. `analysis_source` records the analyzed payload, recorded
SHA256, width and height; `crop_pixels` records the output rectangle in that
source's coordinates. Signal-preview metadata additionally records
`model_origin`. The full-strip fit's diagnostics are shared by all its frames.

### Memory, output, and provenance

The Rust implementation reads sparse rows for analysis, followed by sequential
output strips and optional artifact reads. It does not allocate a full
`H*W` floating-point image. For example, two sets of 768 rows at width 12155
occupy roughly 142 MiB as `f64` for one channel. Currently all channels' sampled
rows are retained during analysis (roughly 427 MiB for RGB at that width, plus
temporary fitting buffers). Channels are fitted sequentially; sampled rows are
released before output. Only the small fitted models remain for export.

The existing TIFF encoder writes 32-row strips, supports Gray8/Gray16/RGB8/RGB16,
uses BigTIFF when necessary, and uses `create_new` to avoid overwriting outputs.
A corrected export retains these properties while transforming each strip.
Cancellation is checked during sampling, fitting, and writing; scan progress
reports a `banding` phase. Correction finishes before intermediate-payload cleanup
in [`src/bin/epscan/retention.rs`](../src/bin/epscan/retention.rs), which verifies
the corrected TIFF and every requested companion, including matching artifact
metadata and file synchronization, before deleting raw intermediates.

Preserve the raw acquisition's identity and hash. A corrected export should
record its own transform provenance: algorithm/version, model, strength, mask
thresholds, axis, ROI, frequencies, phases, map centers, amplitude/support maps,
source reference/hash, output bit depth, requested/assumed DPI, and validation.
Do not label corrected samples as `sample_transform: "none; exact packed samples"`.
Keep raw sharpness measurements distinct from any future corrected-image score.

The current acquisition TIFF writer records JSON description, software, and DPI;
it does not implement arbitrary TIFF import or ICC preservation. An offline TIFF
import feature would separately need decoding, orientation handling, and selected
metadata preservation. The Python prototype preserves ICC and Orientation plus
the requested output DPI, not every arbitrary TIFF tag.

The implementation uses `rustfft` for forward unnormalized FFTs, `nalgebra` SVD
for robust least-squares fitting, and `png` for the diagnostic preview, alongside
the existing `tiff`, `serde`, and `serde_json` dependencies. Numerical operations
use `f64`; altered precision, FFT padding, or solver tolerances can change
borderline detection decisions and require validation.

### Command-line outputs

Add these flags to a normal scan or preview command:

```text
--reduce-banding --save-raw-tiff --save-band-signal
```

With correction enabled, `<basename>_<n>.tiff` is the corrected image. The
optional `<basename>_<n>_raw.tiff` contains the unchanged captured samples, and
`<basename>_<n>_banding.png` shows the signed applied log-gain after the original
darkness mask and strength, before integer quantization and clipping. It is
a diagnostic preview, not a full-resolution numerical difference map. White
means zero; red means positive signal removed (output darker); blue means
negative signal (output brighter). RGB has three channel panels in R/G/B order
on a common symmetric scale. Each panel preserves source orientation and aspect
ratio, and the entire PNG is at most 1600 pixels on either side. Its legend and
numeric range are recorded in PNG text and image metadata.

`--banding-strength 0.8`, `--banding-dark-full 0.1`, and
`--banding-dark-off 0.6` expose the accepted defaults. Optional
`--banding-roi "X0,X1,Y0,Y1"` restricts detection, not the correction area.
These modifier and companion flags require `--reduce-banding`. Scan DPI comes
from the acquisition settings; it does not force a particular physical period.

Existing files are never overwritten. Failure or cancellation during the
banding export removes only artifacts created by that export and retains the
raw acquisition. The default CLI cleanup removes intermediate packed `.bin`
files only after all requested exports have completed; use the existing
`--keep-intermediates` option to retain those packed intermediates as well.

## Validation and diagnostics

Validate using the original held-out integer rows and the **same mask, strength,
clipping, rounding, and integer quantization used for output**. Recompute strip
profiles after correction. At each selected frequency, report:

```text
coherent_before = abs(mean(strip_phasors_before))
coherent_after  = abs(mean(strip_phasors_after))
coherent_reduction_db = 20*log10(before/after)
strip_rms = sqrt(mean(abs(strip_phasors)^2))
residual_projection = real(z_after*conj(z_before)) / abs(z_before)^2
phase_reversed = residual_projection < 0
```

Use numerical floors for zero denominators. Report both coherent and strip-RMS
reductions: opposite residual phases can cancel in the coherent average. If
detection used an ROI, also measure held-out rows and columns within that ROI,
evaluating the correction at their original source coordinates.

These measurements reuse scene content; they are not a ground-truth restoration
score. The current Python code reports failures but **does not automatically
veto or reduce a correction when a metric worsens**. Any automatic acceptance
policy in Rust would be a new feature needing separate validation.

Render diagnostics with the same pixel origin, orientation, and aspect ratio:
original negative; darkness mask; estimated correction at the chosen strength;
and the actual masked correction. Use the same signed color range for the last
two panels. Include the ROI and clearly label columns/x and rows/y. Use one
shared downsampling stride for x and y. Stretching a portrait map into wide plot
axes caused confusion during these trials; never infer correction direction
from the overall side-by-side figure dimensions.

## Reference settings and observed results

All values below use multiplicative correction, strength `0.80`, full mask at
`0.10`, cutoff `0.60`. Each image gets its own fitted model; do not reuse a
previous scan's phase or amplitude map.

| Scan | Width x height | DPI used | Detection ROI: X0 X1 Y0 Y1 | Detected period |
| --- | --- | --- | --- | --- |
| `button_rock_003.tif` | 16912 x 21920 | 4800, confirmed | Full image | 815.473 px / 4.315 mm |
| `2023-09-07-0007.tif` | 12155 x 15000 | 3200, assumed | `8500 12155 10500 14000` | 569.681 px / 4.522 mm |
| `2023-09-09-0002.tif` | 9354 x 11648 | 3200, assumed | `6500 9354 0 11648` | 566.967 px / 4.500 mm |

The user accepted `0007`'s current setup. Its final output retained unsigned
16-bit samples and portrait dimensions; all 71,920,640 source pixels at or above
the 60% cutoff were copied exactly. Its held-out detection-region coherent
amplitude fell by 11.15 dB; full-image coherent reduction was 6.07 dB and strip
RMS reduction was 2.60 dB. The detected model used a 2279-pixel fitting span and
a 12-by-19 amplitude grid.

The earlier `0002` trial improved the measured sky signal by 5.63 dB, but its
whole-image coherent amplitude increased by 2.70 dB. That is a mixed result,
not evidence of improvement everywhere. Its source TIFF was unavailable for
the later final-render request. Use it as a future regression/inspection case
when the original is restored, not as a universally passing fixture.

The threshold change from 30% to 60% was important: the earlier mask almost
excluded the gray cloud in `button_rock_003`. The later mask retained about
77% of the strength-adjusted correction amplitude in the sampled cloud. Earlier
unmasked/full-strength attempts introduced bands in thin areas or reversed the
residual pattern, motivating the original-sample mask and adjustable strength.

Reference command from the sibling workspace:

```powershell
.venv\Scripts\python.exe test3.py 2023-09-07-0007.tif 2023-09-07-0007_final.tif --dpi 3200 --model mult --strength 0.80 --dark-off 0.60 --detection-roi 8500 12155 10500 14000
```

## Validation coverage and remaining limits

Rust tests in `src/scan/banding/{model,tests}.rs` cover frequency and phase
recovery, ROI phase offsets, multiple components, rejection of horizontal or
incoherent patterns, masks, output sample identity, cancellation, collisions,
and signed preview orientation. A deterministic 16-bit fixture compares the
period, phase, amplitude-map values, reconstructed correction and quantized
held-out residuals against Python golden values. Scan and CLI integration tests
also cover acquisition, derived crops, shared captures, optional artifacts and
cleanup. Hardware scanning with this option remains to be checked.

Keep these cases in the regression plan when changing the method:

1. Known vertical sinusoids with noninteger periods and smoothly varying
   amplitudes: recover frequency/phase and reduce their residuals. Include
   horizontal-only signals to catch an accidental axis swap.
2. A nonzero detection-ROI x origin and a derived-frame crop: reconstruct the
   carrier correctly on both sides of the detection crop and after output
   cropping. Test original y selection rather than sampled-row indices.
3. Several separated and nearby frequencies: exercise joint fitting, component
   selection, and fitting-window expansion.
4. Constant images or no supported band: no false selected band and unchanged
   output. Unsupported sample formats or malformed inputs: explicit errors.
5. Zero pixels, full-scale pixels, mask boundaries, and halfway rounding cases:
   preserve zeros in multiplicative mode and exact protected pixels in both
   modes, and match integer quantization.
6. RGB with unequal channel brightness: one mask from the original maximum
   channel, independent of channel-processing order.
7. Overestimated amplitude and reduced strength: verify that halving a known
   doubled estimate does not introduce the opposite periodic pattern.
8. File-level output: correct packed byte order, original dimensions/bit depth,
   requested DPI, no source overwrite, cancellation, and truthful provenance.

Existing Python checks are in `test_16bit.py`, `test_local_banding.py`, and
`test_detection_roi.py`. All 31 checks passed after adding ROI detection. Capture
additional intermediate spectra, selected carriers, map values, and corrected
sample rows as small golden data; tolerate normal solver/FFT
roundoff rather than requiring byte-identical floating-point arrays.

The main remaining limitation is separation of scene content from banding.
Repeated scene edges and clouds can influence local amplitude even when the
frequency was learned in clear sky. The amplitude cap and phase/noise shrinkage
reduce that risk but do not eliminate it. Shared phase cannot fully represent
curved bands, slanted bands, or phase drift with y. A higher ROI confidence does
not prove that extrapolation into every other part of the negative is correct.
Retain visual before/after and applied-correction inspection while developing
and tuning the Rust implementation.
