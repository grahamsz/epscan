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
- [Epson V800 specifications](https://download4.epson.biz/sec_pubs/epson_perfection_v800_photo/useg/en/html/specs_2.htm)
  for optical versus output DPI. Optical-path interpretation remains an inference;
  a focus acknowledgement is not proof of physical lens selection.
