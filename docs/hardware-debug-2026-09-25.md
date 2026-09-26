# V800 start-rejection investigation, 2026-09-25

This note covers trials 01-13. Trial 11 observes the scanner entering its
reported warmup state after a rejected first start. The resulting bounded
recovery completed a production acquisition after a fresh user-confirmed
power cycle in trial 12. The warm repeat in trial 13 needed only one start.
This validates that recovery on one controlled cold cycle of the attached unit;
it does not establish every source, mode or firmware combination.

The attached scanner reports GT-X980, ESC/I B8, firmware 1.10, USB `04b8:0151`.
These are new tests of the converted `epscan` implementation on Windows using
the installed `usbscan.sys` driver. They are separate from the earlier sibling
`../epson` reference captures.

Local manifests, raw payloads, protocol JSONL, and command output are retained
under `captures/hardware-debug-20260925/`. These artifacts are not release
contents. The tables below retain settings, counts and outcomes without USB
instance identifiers, original image samples, or private application paths.

## Trial matrix

All acquisitions below request primary TPU/film-holder source, RGB16,
identity gamma tables, preview byte 0, and 32 lines per block. The production
sequence includes ESC @, FS W, focus 89, R/G/B LUT uploads, FS S readback,
FS F readiness, and FS G. Through trial 11 no retry was performed within a
trial; trial 12 exercises the implemented bounded recovery.
Rectangles are x, y, width, height in millimetres. Time is the acquisition
interval recorded in the manifest, not total application runtime.

| Trial / manifest stem | Context | DPI | Rectangle mm | Effective pixels | Image bytes | Acquisition s | Result |
| --- | --- | ---: | --- | --- | ---: | ---: | --- |
| 01 `01-baseline_1` | Initial production baseline; no controlled cold start | 300 | 0,0,10,10 | 112 x 118 | 79,296 | 8.117 | Complete |
| 02 `02-repeat_1` | Same production settings | 300 | 0,0,10,10 | 112 x 118 | 79,296 | 7.009 | Complete |
| 03 `03-repeat_1` | Same production settings | 300 | 0,0,10,10 | 112 x 118 | 79,296 | 7.068 | Complete |
| 04 `04-repeat_1` | Same production settings | 300 | 0,0,10,10 | 112 x 118 | 79,296 | 7.068 | Complete |
| 05 `05-highdpi_1` | Small region at higher DPI | 3200 | 0,0,10,10 | 1256 x 1260 | 9,495,360 | 31.098 | Complete |
| 06 `06-lowdpi-after-high-vuescan-closed_1` | User confirmed VueScan closed; return to 300 DPI | 300 | 0,0,10,10 | 112 x 118 | 79,296 | 8.132 | Complete |
| 07 `07-wide-300_1` | Larger production region | 300 | 10,10,100,100 | 1176 x 1181 | 8,333,136 | 32.484 | Complete |
| 08 `08-cold-baseline_1` | First production attempt after user-reported power cycle; VueScan closed | 300 | 0,0,10,10 | 112 x 118 planned | 0 | Not started | FS G 0x92 |
| 09 `09-cold-control` | Diagnostic harness, approximately 21 seconds after 08; no further power cycle | 300 | 0,0,10,10 | 112 x 118 | 79,296 | 9.934 | Complete |
| 11 `11-settled-cold-observe` | First start after user-reported power cycle and restored USB discovery; status-only observation enabled | 300 | 0,0,10,10 | 112 x 118 planned | 0 | 0.002 to rejection | FS G 0x92, then warmup-to-ready transition |
| 12 `12-fixed-cold_1` | Rebuilt production CLI, fresh user-confirmed power cycle; VueScan closed | 300 | 0,0,10,10 | 112 x 118 | 79,296 | 12.363 | Complete after confirmed warmup and one resumed start |
| 13 `13-fixed-warm-repeat_1` | Same rebuilt production CLI and settings; no further power cycle | 300 | 0,0,10,10 | 112 x 118 | 79,296 | 7.019 | Complete with one start |

There are 12 acquisition trials: ten completed and two deliberately unrecovered
baseline/observation attempts failed. Trial 10 is excluded because it issued
no scanner commands.

Trial 09 used the default experimental-harness sequence: reset and focus
enabled, identity LUT after parameters, no explicit ESC e, no deliberate
settling delay, and the same parameter bytes as trial 08. Its harness also
queried FS F and FS S before reset. These extra queries, elapsed time, and the
preceding failed attempt are confounded; 09 is not a clean single-variable
comparison with 08.

Trial 10 was scheduled after another user-reported power cycle and a requested
60-second wait. Device discovery returned zero scanners before opening a
transport or issuing any scanner command. It is not an acquisition trial and
provides no additional evidence about scan-start behavior.

Trial 11 follows restored USB discovery after the user reported a power cycle
and was asked to wait 60 seconds. The exact interval from the last physical
power action was not measured, and a steady ready light was not explicitly
reported. Its setup uses the same default
harness settings as 09, plus `--observe-rejection`. Only one FS G was sent;
the observer queried status after rejection and never retried the acquisition.

## What the traces establish

Trials 01, 02, 03, 04, 06 and 08 have identical first 27 direction/payload
records through the FS G request, including LUT contents, parameter readback,
and ready status. Those records also match the initial converted-CLI failure
in `dist/scan_1.protocol.jsonl` and a successful RGB reference acquisition.
The same transmitted settings can therefore precede either acceptance or
rejection; these captures do not identify a conversion-induced packet change.

Trial 08 reports ready status `0100c000000000000080000000000000`, followed by
the complete rejected-start header `0292000000000000000000000000`. Its partial
payload is empty. The header arrives approximately 1.324 ms after FS G.
Trial 09's initial FS F is identical, and its initial FS S matches the settings
left by 08. Its accepted header arrives approximately 7.249 seconds after FS G.
This separates the immediate refusal from transfer/stride or image-export
failure. The rejected header sets the fatal flag and advertises zero image
lengths; it does not itself identify a root cause.

Successful small 300-DPI production trials need no explicit host sleep:
01/02/06 reset-to-start intervals are about 29.1 ms, close to 08 and the original
failure. Some other successes wait nearly an additional second for reset ACK.
Neither observation establishes a required one-second delay.

The 32-line full blocks in 05 and 07 contain 241,152 and 225,792 image bytes,
respectively. Both complete, so a universal 64 KiB transfer ceiling is not an
explanation for the failures. Trial 07 also matches the FS W parameters of a
previously failed large-region reference scan. These results do not establish
that every line count, geometry, or cold-start condition is reliable.

The manifests record byte counts and payload hashes for completed production
passes. All nine production TIFFs from trials 01-07 and 12-13 were independently decoded
as RGB16 and their samples serialized to little-endian bytes: every decoded
byte matched its raw payload, and dimensions matched the manifests. Independent
raw-file length and SHA-256 checks also matched all nine manifests. The successful
diagnostic harness trial 09 produces raw data without a TIFF.
Completion here means complete protocol/image transfer and faithful TIFF
sample export; it does not
establish measured optical resolution, tone linearity, focus accuracy, or
spectral infrared identity. None of these trials acquires infrared.

## Trial 11: preparation begins after the first start

The decisive sequence is retained in `11-settled-cold-observe.json` and its
protocol trace:

| Event | Device response | Interpretation |
| --- | --- | --- |
| Before setup | FS F `01008000000000000080000000000000` | No warmup reported; primary TPU installed |
| After setup/readback, before FS G | FS F `0100c000000000000080000000000000` | No warmup reported; primary TPU enabled |
| First FS G | `0292000000000000000000000000` | Complete rejected-start header, zero image lengths |
| First post-rejection FS F | `0300c000000000000080000000000000` | Warmup flag set; no fatal, busy, TPU error or lid-open flag |
| Status observations through 2.513 s | Same `03...` response | Reported warmup remains set |
| Observation completed at 2.764 s | `0100c000000000000080000000000000` | Warmup clears and ready status returns |

The elapsed status times are relative to the start of observation. Polling was
every 250 ms, so the transition occurred between the final warmup observation
and the first ready observation; 2.764 seconds is not a sub-millisecond estimate
of the actual preparation duration. The first post-rejection response arrived
about 10 ms after the rejected header. The raw payload remains empty.

This directly demonstrates a start-triggered, device-reported warmup transition
on this GT-X980/B8 unit after FS F had reported ready. It supports a narrowly
scoped retry after that transition, rather than an arbitrary delay before the
first start. It does not establish the behavior of every firmware, source or
mode, or explain the unrelated unexpected-ACK and IR entry failures.

## Trials 12-13: production recovery and warm repeat

After another user-confirmed power cycle with VueScan closed, trial 12 used the
rebuilt production CLI with the original parameters and setup sequence. Its
first FS G returned the exact zero-length 0x92 rejection. Immediate status
reported warmup; polling observed healthy ready status after 2.766 seconds.
One additional FS G then returned `02120054000003000000c0390000`, advertising
three 21,504-byte blocks and a 14,784-byte tail. All 79,296 image bytes arrived.
The manifest records `start_attempts: 2`, the rejected header, every status
observation, and `warmup_seconds: 2.7659114`; total acquisition time was
12.362787 seconds. No reset or parameter upload occurred between starts.

Trial 13 repeated the same production settings without a power cycle. It
completed the same-sized image in 7.018901 seconds with `start_attempts: 1`
and `start_recovery: null`. Together these results demonstrate the intended
cold-start recovery and unchanged single-start behavior on a warm scanner.
The cold-start validation covers one power cycle, one unit and this RGB16
holder-source geometry; it is not evidence that the separate IR failures are
resolved.

## VueScan comparison and provenance

The user provided a VueScan 9.8.16.02 diagnostic log, read without changing it.
The snapshot contains 100,140 bytes and has SHA-256
`328fb06a156ffb37f0c33983d922432a8e6734a59f365c7a87a68bd9c9a0e78a`.
The original log is not committed. Sanitized observations are retained locally
in `captures/hardware-debug-20260925/vuescan-observations.md`; that note's
SHA-256 when this draft was prepared was
`08e98a8f809f27d86d8f0a85f293fab6938472c3dd3920dc54076773a9611ba6`.

That log contains nine accepted FS G headers, all with status 0x12 and nonzero
transfer lengths. Two acquisitions are manually cancelled after receiving
blocks; seven reach their advertised final reads. This does not demonstrate
VueScan behavior from a controlled power-on state.

The observed differences from the baseline are useful experimental candidates:

- Before the first acquisition VueScan sends ESC e with value 1, identity
  R/G/B tables, and an identity color matrix. No ESC @, ESC p, FS S, or IR-enable
  transaction appears in the recorded interval.
- All nine parameter packets use 16-bit output, preview/speed byte 1, custom
  gamma mode 3, and zeroes at bytes 30-63. Baseline epscan uses preview 0,
  halftone byte 32 = 1, and threshold byte 33 = 128.
- Parameter ACK and FS F/start fall in successive logged seconds. The log's
  one-second timestamps cannot determine an exact delay or its purpose.
- Holder scans are monochrome; area-guide scans are RGB. The two recorded
  holder final scans use 3200 x 9600 acquisition DPI and three acquired rows
  per output row despite the UI's sample setting of 16.
- The observed block-line counts fit the largest even count keeping a full
  block below 65,536 image bytes. This is an inference from these captures,
  not a copied algorithm or a demonstrated scanner requirement.
- Nonfinal blocks are ACKed, cancellations use CAN/ACK at block boundaries,
  and no extra final ACK is shown. This agrees with epscan's transfer handling.

No one of these differences is established as necessary. In particular, trial
09 succeeds without ESC e, so its absence cannot explain every accepted versus
rejected start. Cold-start source selection still needs its own comparison.

## Primary-source context for the observed transition

Epson's published ESC/I implementation checks warmup before acquisition but
also handles devices whose preparation begins only when a scan is requested.
Following a fatal start response, it checks status again, waits while warmup
is reported, and then attempts another start. See the pinned
[Image Scan extended-scanner implementation](https://github.com/utsushi/imagescan/blob/fcaaaf5d5c8b5bcb4aa22d9cd75398d3401738b4/drivers/esci/extended-scanner.cpp#L558-L579).
SANE independently handles an extended-start error by checking warmup before
a second start in its pinned
[epson2 start path](https://gitlab.com/sane-project/backends/-/blob/cadda80b9fec0d69728561aeae57eb8278f99c75/backend/epson2.c#L2268-2277).
These are protocol-behavior references; their implementations are not copied.

Trial 11 now confirms the reported warmup transition on the attached V800.
It does not justify retrying arbitrary errors. FS F byte 0 bit 0x02 is the reported warmup flag;
bit 0x01 reports warmup-cancellation support. These meanings must not be
transferred to similarly numbered bits in the FS G header.

Epson describes the V800's LED source as requiring no lamp warmup, so the
observed condition is device-reported preparation/readiness rather than a
demonstrated physical lamp-heating delay. Epson separately identifies a
flashing green scanner light as initialization/scanning and a steady green
light as ready. See the [V800 product description](https://epson.com/For-Work/Scanners/Photo-and-Graphics/Epson-Perfection-V800-Photo-Color-Scanner/p/B11B223201)
and [V800 light-status guide](https://support.epson-europe.com/onlineguides/en/perfv800/html/parts_2.htm).

ESC e selects a source/mode and resets the scan area; it belongs before the
final geometry request. FS W also carries source selection. ESC @ resets scan
settings but retains uploaded LUT/matrix/dither data and focus position.
Neither operation is documented as a substitute for a physical power cycle.
See Epson's pinned [source-selection documentation](https://github.com/utsushi/imagescan/blob/fcaaaf5d5c8b5bcb4aa22d9cd75398d3401738b4/drivers/esci/setter.hpp#L187-L204)
and [initialization documentation](https://github.com/utsushi/imagescan/blob/fcaaaf5d5c8b5bcb4aa22d9cd75398d3401738b4/drivers/esci/initialize.hpp#L33-L45).

## Implemented recovery and remaining validation

The production recovery is gated by the model profile. It may
issue at most one additional FS G only after consuming the exact 0x92 header
with all three transfer lengths zero, observing the warmup flag, and then
observing healthy ready status. It retains the same pass deadline and does not
reset or resend settings between attempts. Missing warmup, malformed headers,
unrelated errors, fatal/source/lid-open status, busy status after warmup,
a second rejection, cancellation, and timeout remain failures. Low-level
`Esci::acquire` remains strict; the normal session uses `acquire_with_policy`
with `TransferQuirks::allow_start_warmup_recovery` from the registered model.

The final offline suite passed 72 tests: 64 library, seven CLI and one doctest;
formatting and strict Clippy checks also passed. Regressions cover the accepted transition, exact rejection framing,
strict/disabled policy, partial replies, unhealthy status, second rejection,
cancellation and the shared deadline. Hardware trial 12 confirms the recovery
path and complete image; trial 13 confirms the ordinary one-start path.
Further source, line-count, firmware and setup variants remain independent
experiments. No fixed delay, block-size, source-enable or processing-parameter
change was required for this fix.
