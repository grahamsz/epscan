"""Hardware-free extension import/argument smoke checks."""
from importlib.metadata import version
from pathlib import Path
import tomllib

import epscan

with (Path(__file__).resolve().parents[1] / "Cargo.toml").open("rb") as manifest:
    expected_version = tomllib.load(manifest)["package"]["version"]
assert epscan.__version__ == expected_version
assert version("epscan") == expected_version
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
