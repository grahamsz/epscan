# Changelog

## 0.1.0

- Converted the forked nkscan project to an Epson ESC/I library named `epscan`.
- Added a central model capability registry for the V800/V850 / GT-X980 family.
- Added true 8/16-bit grayscale acquisition with strip extraction and sharpness scoring.
- Added Epson USB transports, acquisition, TIFF/raw results, CLI and Python API.
- Removed Nikon-specific code, controls, profiles, documents and tooling.
- Added offline packet, validation, transport, cancellation, and image tests.
- Documented the local Epson reference, external protocol sources, and outstanding
  hardware validation (especially infrared and full-resolution acquisition).
