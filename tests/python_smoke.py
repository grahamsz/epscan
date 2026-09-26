"""Hardware-free extension import/argument smoke checks."""
import epscan

assert epscan.__version__ == "0.1.0"
assert issubclass(epscan.ScanCancelled, epscan.ScannerError)
assert issubclass(epscan.DeviceBusy, epscan.ScannerError)
assert callable(epscan.Session.plan_regions)
assert callable(epscan.Session.scan_regions)
for operation in (
    lambda: epscan.list_devices("invalid-backend"),
    lambda: epscan.Session(timeout=0),
    lambda: epscan.Session(timeout=float("nan")),
    lambda: epscan.Session(backend_name="invalid-backend"),
):
    try:
        operation()
    except ValueError:
        pass
    else:
        raise AssertionError("invalid argument was accepted")
print("epscan Python import and argument checks passed")
