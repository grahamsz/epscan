# Research sources

This conversion uses the original Rust and documented device observations from
`../epson`. That sibling directory is not needed to compile or run `epscan`.

| Reference | Revision / evidence | Used for |
| --- | --- | --- |
| [SANE backends](https://gitlab.com/sane-project/backends) | `cadda80b9fec0d69728561aeae57eb8278f99c75` | epson2 command fields, USB handshakes, identity, transfer termination, IR challenge |
| [Epson Image Scan / Utsushi](https://github.com/utsushi/imagescan) | `fcaaaf5d5c8b5bcb4aa22d9cd75398d3401738b4` | ESC/I scan-parameter, focus, brightness and gamma documentation |
| [VueScan December 2022 newsletter](https://www.hamrick.com/newsletter/December-2022.html) | Ed Hamrick's published Epson sampling notes | Independent X/Y resolution and averaging acquired rows; reported V600/V850 tests |
| V800 USB identity and acquisition records | Reference and converted-library tests on 2026-09-24/25 | GT-X980/B8 identity, geometry, RGB transfers, IR failures, gamma failure regression, and observed start-triggered preparation |

SANE files inspected in the reference included epson2.c/.h, epson2-commands.c/.h,
epson2-ops.c, epson2-io.c, epson2_usb.c, and earlier epson.c. The protocol was
implemented independently in Rust.
see [NOTICE](../NOTICE.md).

The converted-library investigation now records a first-start rejection followed
immediately by FS F warmup status, which clears during a 2.764-second observation.
Epson's pinned [extended-scan implementation](https://github.com/utsushi/imagescan/blob/fcaaaf5d5c8b5bcb4aa22d9cd75398d3401738b4/drivers/esci/extended-scanner.cpp#L558-L579)
describes devices that enter preparation only after a start request; SANE's
[extended-start handling](https://gitlab.com/sane-project/backends/-/blob/cadda80b9fec0d69728561aeae57eb8278f99c75/backend/epson2.c#L2268-2277)
provides a second behavioral reference. These references informed the bounded
recovery design; no implementation code was copied. The implemented recovery
completed a production RGB16 scan after a fresh user-confirmed power cycle in
trial 12; the warm repeat completed with one start in trial 13. This verifies
one controlled cold cycle on the attached unit.
[The investigation note](hardware-debug-2026-09-25.md)
records the evidence, limitations, and SHA-256 provenance of the user-provided
VueScan log; the original log is not committed.

Additional primary references:

- Visible grayscale: pinned Image Scan
  [color-mode constants](https://github.com/utsushi/imagescan/blob/fcaaaf5d5c8b5bcb4aa22d9cd75398d3401738b4/drivers/esci/constant.hpp#L118-L130)
  identify monochrome as `0x00`, distinct from RGB `0x13`. Its
  [scanner implementation](https://github.com/utsushi/imagescan/blob/fcaaaf5d5c8b5bcb4aa22d9cd75398d3401738b4/drivers/esci/extended-scanner.cpp#L1661-L1680)
  recognizes Gray8 and Gray16. The local reference note
  `../epson/docs/vuescan-log-2026-09-25.md` records completed mono16 acquisitions
  on this unit with normal TPU source 1 and exactly `744 * 2157 * 2` image bytes.
  This informed the independent grayscale mode implementation; it does not
  constitute a hardware test of epscan's new grayscale path. Optional custom
  gamma keeps the existing R/G/B identity-table uploads, consistent with SANE's
  mode-independent uploads and the supplied VueScan mono trace. Image Scan's
  [gamma-table documentation](https://github.com/utsushi/imagescan/blob/fcaaaf5d5c8b5bcb4aa22d9cd75398d3401738b4/drivers/esci/set-gamma-table.hpp)
  remains a protocol reference; no upstream implementation was copied.

- Transfer-size observations: pinned SANE
  [epson2 block setup](https://gitlab.com/sane-project/backends/-/blob/cadda80b9fec0d69728561aeae57eb8278f99c75/backend/epson2-ops.c#L1235-1285)
  derives line count from row size and a 128 KiB USB budget, with additional
  model-specific rules. Pinned Image Scan
  [transfer setup](https://github.com/utsushi/imagescan/blob/fcaaaf5d5c8b5bcb4aa22d9cd75398d3401738b4/drivers/esci/extended-scanner.cpp#L931-L981)
  also considers row size and its buffer limit (256 KiB by default).
  The supplied VueScan trace includes 65,472-byte monochrome blocks.
  These informed an independent, conservative 64 KiB target in epscan; no
  implementation was copied. They do not prove a cause or fix for the observed
  long-strip timeout, nor a 64 KiB USB requirement.

- [nusb 0.2.7](https://docs.rs/nusb/0.2.7/nusb/) and locally compiled dependency source.
- [Microsoft USB scanner driver](https://learn.microsoft.com/en-us/windows-hardware/drivers/image/usb-driver),
  [IOCTL_SET_TIMEOUT](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/usbscan/ni-usbscan-ioctl_set_timeout),
  installed SDK usbscan.h, and the installed Epson driver's INF.
- [Epson V800/V850 user guide](https://files.support.epson.com/docid/cpd4/cpd41530.pdf)
  for placement and infrared film restrictions.
- [Epson's 4 × 5 inch film placement guide](https://files.support.epson.com/docid/cpd4/cpd41530/source/scanners/source/placing_originals/tasks/placing_45_film_pv800_v850.html),
  consulted 2026-09-26, for the official holder name and placement. The registered
  `v800-4x5` rectangle comes from a local loaded-holder 300-DPI RGB8 preview on
  that date, not dimensions inferred from the guide; see [holder measurements](holders.md).
- [Epson V800 specifications](https://download4.epson.biz/sec_pubs/epson_perfection_v800_photo/useg/en/html/specs_2.htm)
  for optical versus output DPI. Optical-path interpretation remains an inference;
  a focus acknowledgement is not proof of physical lens selection.
- [Epson V800 hardware-resolution specifications](https://epson.com/For-Work/Scanners/Photo-and-Graphics/Epson-Perfection-V800-Photo-Color-Scanner/p/B11B223201),
  consulted 2026-09-26: 4800 x 9600 and 6400 x 9600 DPI hardware resolutions,
  compared with 12800 x 12800 maximum output resolution. These establish the
  model's 9600 DPI carriage-axis oversampling cap; identity limits can lower it.
  Extra carriage samples are averaged, not assumed to provide calibrated
  independent exposures or a measured noise improvement.
- [Epson medium-format placement guide](https://files.support.epson.com/docid/cpd4/cpd41530/source/scanners/source/placing_originals/tasks/placing_medium_film_pv800_v850.html)
  and [Epson V800 product specifications](https://epson.com/For-Work/Scanners/Photo-and-Graphics/Epson-Perfection-V800-Photo-Color-Scanner/p/B11B223201),
  consulted 2026-09-26, for the official medium-format holder and its 6 x 20 cm
  maximum film size. The registered opening comes from a local empty-holder
  300-DPI RGB8 preview, not from dimensions inferred from the guide. No loaded
  medium-format film was available for exposure-registration tests.
- [Pentax 645NII specifications](https://www.ricoh-imaging.co.jp/english/products/filmcamera/medium/645n2/spec.html),
  consulted 2026-09-26, for the 56 x 41.5 mm image-size example used by the 6x4.5
  starter preset. Other medium-format sizes and all nominal frame gaps are
  editable assumptions, not claims of universal camera-gate dimensions.

- Older scanner registry (consulted 2026-09-26): pinned SANE
  [epson2 device descriptions](https://gitlab.com/sane-project/backends/-/blob/cadda80b9fec0d69728561aeae57eb8278f99c75/doc/descriptions/epson2.desc)
  provide V500/V550/V600 USB IDs and their epkowa/non-free interpreter requirement,
  and V700/V750 USB ID, GT-X900 identity and epson2 support classification.
  The same revision's `backend/epson2-ops.c` groups GT-X900 with GT-X980 for
  secondary transparency-source and transfer handling. These are protocol
  references, not local V700 hardware verification.
- [Epson V700 specifications](https://www.epson.co.in/For-Home/Scanners/A4-Home-Photo-Scanners/Epson-Perfection-V700-Photo/p/B11B178026)
  establish RGB/grayscale output depths and 9600-DPI carriage-axis hardware
  resolution for the provisional profile.
- [Epson slide placement guide](https://support2.epson.net/manuals/english/scanner/perfectionv800pv850p/use_g/html/set2_2.htm)
  and the V800/V850 user guide document the official twelve-position 35 mm Slide
  Holder. Coordinates were measured locally, including a loaded portrait and
  landscape check; see [holder measurements](holders.md).
