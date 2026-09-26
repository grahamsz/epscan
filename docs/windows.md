# Windows transport, setup and recovery

## Keep the installed Epson driver

In the original reference hardware session, the scanner reported hardware ID 04b8:0151, friendly name
EPSON Perfection V800/V850, Epson driver 1.1.0.0 dated 2025-04-14,
INF oem18.inf and service `usbscan`. The INF includes sti.inf's USB service.
No driver was changed.

The Rust transport enumerates imaging interfaces with SetupAPI, opens the
matching device exclusively using CreateFile (share mode zero), validates its
descriptor/bulk pipes, and exchanges ESC/I using ReadFile/WriteFile.
No WIA, STI COM, TWAIN or vendor scanning API is invoked. Microsoft's
[USB scanner driver contract](https://learn.microsoft.com/en-us/windows-hardware/drivers/image/usb-driver)
supports these operations. The tested endpoints are OUT 01 / IN 82, packet
size 512. IOCTL indexes 6, 10 and 11 query the descriptor, pipe layout and set
timeouts. The imaging GUID is 6bdd1fc6-810f-11d0-bec7-08002be2092f.

Build with Rust and Visual Studio C++ tools; run `epscan list`, then
`epscan dump`. Close other scanning applications first. Check the
transport lock, lid cable and film illumination setup, and leave the
calibration area unobstructed. The library leaves firmware calibration intact.

`--backend auto` prefers usbscan on Windows. Dropping/closing the session
releases the handle. Driver coexistence means the installed binding remains
usable; concurrent scanner clients are not supported. Epson Scan/SilverFast
operation after release was not exercised in that reference session.

## nusb and optional WinUSB binding

[nusb](https://docs.rs/nusb/0.2.7/nusb/) uses WinUSB on Windows. It successfully
enumerated this scanner, but opening through nusb reported an incompatible
driver under the stock binding. This is expected; a user-space crate cannot
make usbscan.sys into WinUSB. Use the default transport with this binding.

The nusb transport discovers a unique bulk IN/OUT pair, claims its interface,
performs blocking bulk transfers, rounds IN buffers to USB packet size and
retains any surplus bytes between reads. It does not install/detach drivers.
Actual image transfer through nusb is not tested yet.

Only if deliberately switching to WinUSB:

1. Record the scanner's exact hardware ID, provider, version and INF. Obtain
   the official Epson installer first. Optionally export that exact package
   with elevated `pnputil /export-driver oem18.inf C:\DriverBackup\EpsonV800`;
   the INF name is machine-specific.
2. Use official [Zadig](https://zadig.akeo.ie/), List All Devices, select only
   04b8:0151, and manually choose WinUSB. Verify the exact device before applying.
3. Expect Epson Scan/SilverFast to lose access with that binding. This is not
   simultaneous driver coexistence.
4. Restore through Device Manager: Roll Back Driver, or Update Driver > Browse
   > Let me pick the original Epson driver. If missing, use Have Disk with the
   exported INF or reinstall Epson's official package. Reconnect and verify
   the Epson/usbscan binding before using vendor applications.

No step above was performed automatically. UsbDk/filter drivers were considered
during Python research but require additional system drivers, are not nusb
backends here, and were unnecessary because native usbscan bulk access worked.
No new kernel driver is needed.

## Failure handling

| Symptom | Response |
| --- | --- |
| Nothing listed | Check power, cable, hardware ID and selected backend |
| Busy / Windows sharing violation | Close other clients and reopen |
| Access denied / driver error | Check process access and actual driver binding |
| NAK or parameter mismatch | Keep trace and requested settings; no silent fallback |
| FS G 0x92 | Start rejected; no automatic retry. Ready status alone is insufficient |
| Unexpected reply, e.g. 02 instead of ACK | Session closes; preserve trace, stop commands, power cycle manually if out of sync |
| Timeout/disconnect | Partial payload and manifest remain; reopen only after checking hardware |

A real IR repeat desynchronized replies; the user power-cycled the scanner.
Diagnostics then recovered. A small RGB scan failed with 16-line blocks;
32-line blocks succeeded twice. The firmware's precise constraint remains
unresolved. See the hardware report rather than assuming every error is busy.

The user also observed 02 during a 3200-dpi RGB setup, after the red identity
gamma payload. The scan parameters and focus had been acknowledged, and no
scan-start command was sent. Updated errors identify the command and payload;
this does not yet resolve the firmware/transport failure. Subsequent USB
identity and reset attempts timed out. Power-cycle before further testing.

`--io-timeout` defaults to 60 seconds per response/block (maximum 214);
usbscan rounds to whole seconds. Each transfer read/write is bounded by the
remaining `--scan-timeout`, subject to that whole-second rounding. Ctrl+C is
deferred to a complete block boundary.
No CAN is sent into unknown framing after a mid-block communication error.
Releasing the handle does not prove the carriage has already stopped.
