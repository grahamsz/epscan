Initial epscan 0.1 release with CLI binaries for Windows x64, Linux x64, and macOS Intel/Apple Silicon.

Includes holder layouts, batched and incremental frame delivery, Y oversampling, scanner metadata, and experimental backlight banding correction. V800/V850 is the tested scanner family; V700/V750 support is provisional. V500/V550/V600 are recognized but require an interpreter that is not implemented.

Extract the archive and run `epscan --help`. Windows uses the installed Epson USB scanner driver. Linux may require USB device permissions. macOS binaries are unsigned and not notarized. SHA-256 checksums are provided.

Implementation and release setup assisted by Codex.
