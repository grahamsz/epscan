epscan 0.1.1 fixes holder numbering and the default scan deadline.

- Reverse the V800/V850 35 mm strip order: former strip 3 is now strip 1, frames 1-6. Half-frame numbering follows the same order. Frame order within each strip is unchanged. Clear saved holder framing in NegPy after updating.
- Allow one hour per scan pass by default, retaining the separate 60-second response/block timeout. This accommodates oversampled large-format scans; it does not resolve the intermittent incomplete USB transfer.

Extract the archive and run `epscan --help`. Windows uses the installed Epson USB scanner driver. Linux may require USB device permissions. macOS binaries are unsigned and not notarized. SHA-256 checksums are provided.

Implementation and release setup assisted by Codex.
