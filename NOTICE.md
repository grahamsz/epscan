# Provenance and acknowledgements

This repository began as a clone of [activexray/nkscan](https://github.com/activexray/nkscan),
by Kira Shila and contributors, under MIT OR Apache-2.0. Its layered library
organization and CLI conventions informed this conversion. The Nikon scanner
implementation, SCSI/FireWire tooling, vendor ICC profiles, and Nikon protocol
documents have been removed. Epscan uses the MIT option of that license; the original MIT license and attribution are retained.

The CLI retains the original fork's progress coordinator unchanged, along with
its pass-bar formatting, tracing setup and generic cancellation UI. Small
adapters connect these to Epson passes and safe cancellation points.

SANE's Epson backends were consulted to decode ESC/I command bytes, field
meanings, handshakes, identity data, and the infrared challenge.

Epson's Image Scan/Utsushi source documentation was also consulted, including
[set-gamma-table.hpp at fcaaaf5](https://github.com/utsushi/imagescan/blob/fcaaaf5d5c8b5bcb4aa22d9cd75398d3401738b4/drivers/esci/set-gamma-table.hpp).
It documents custom tone tables and their persistence across initialization.

Ed Hamrick's [VueScan December 2022 newsletter](https://www.hamrick.com/newsletter/December-2022.html)
describes Epson sampling through independent X/Y resolution and row averaging.
User-provided VueScan wire observations in the reference project also informed
our protocol understanding.

[docs/sources.md](docs/sources.md) records revisions, links, and the distinction
between protocol evidence and hardware verification. Cargo dependencies retain
their own licenses.
