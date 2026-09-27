# negative-banding

Rust engine owned by epscan, with an independent copy in the Photoshop plugin. It has no scanner, TIFF, Photoshop, UXP, or network runtime
dependencies. Detection, signed residual refinement, darkness weighting, and
the pure-sine layer/mask renderer live here. Changes between the two repositories
are ported explicitly.

`analyze_samples` accepts normalized, disjoint training and held rows. `analyze`
reads those samples from a packed 16-bit file. Both retain the detected fixed
frequencies/phases and conservatively refine local amplitudes. `sample_rows`
supplies the shared sampling schedule; cancellation is an `AtomicBool`.

`Analysis::correction_region_row` supplies full-precision, refined signal in
source coordinates for crop-safe TIFF export. `linear_corrected_sample` applies
strength and darkness, clamps, and rounds once at the requested bit depth.
`Analysis::render_compact` and its parallel version encode editable pure sine
layers/combined masks and predict Photoshop's quantized composition. Their
0..32768 representation is intentionally confined to the layer output.

The TIFF and Photoshop outputs share the fit but need not be bit-identical:
Photoshop quantizes carriers, masks and intermediate blends, and clips components
sequentially. Direct TIFF export retains full 8/16-bit precision and clips the
summed correction once. Weak residual regions retain their initial amplitudes;
the 10%-to-60% darkness guard remains part of the response model.

The core, epscan, and the plugin use 100% fitted strength by default.
Photoshop's 66% opacity is mask
normalization/headroom, not an extra 0.66 factor for TIFF correction.

Run `cargo test -p negative-banding` from the epscan workspace. The crate includes
the original detector's Python golden values, residual under/over-application
tests, layer compositing checks, and TIFF-vs-layer/crop precision checks.

epscan uses this workspace crate through a local path dependency. The Photoshop
plugin owns its engine in its own repository and does not check out epscan.
Builds and releases are independent.

License: MIT. The original detector was developed in epscan;
residual refinement and layer rendering were developed in photoshop-banding.
