from typing import Any, Callable, Sequence, TypedDict
from types import TracebackType

__version__: str

class Settings(TypedDict, total=False):
    source: str
    mode: str
    dpi: int
    y_oversampling: int
    depth: int
    rect_mm: list[float]
    gamma: str
    preview: bool

class BandingOptions(TypedDict, total=False):
    strength: float
    dark_full: float
    dark_off: float
    max_frequencies: int
    min_period: float
    max_period: float | None
    window_cycles: float
    grid_y: int
    detection_roi: list[int] | None
    save_raw: bool
    save_signal: bool

class Options(TypedDict, total=False):
    infrared: bool
    infrared_only: bool
    ir_depth: int
    ir_gamma: str
    thumbnail: bool
    export_tiff: bool
    banding: BandingOptions | None
    film: str
    pass_timeout: float
    settle_seconds: float

class ScannerError(RuntimeError): ...
class DeviceNotFound(ScannerError): ...
class DeviceBusy(ScannerError): ...
class UnsupportedError(ScannerError): ...
class ScanCancelled(ScannerError): ...

def list_devices(backend_name: str = "auto") -> list[dict[str, Any]]: ...

class Session:
    def __init__(self, device: str | None = None, *, backend_name: str = "auto", timeout: float = 60.0) -> None: ...
    def capabilities(self) -> dict[str, Any]: ...
    def diagnostics(self) -> dict[str, Any]: ...
    def scan(self, basename: str, *, settings: Settings | None = None, options: Options | None = None,
             progress: Callable[[str, int, int, int], bool] | None = None) -> dict[str, Any]: ...
    def plan_regions(self, regions_mm: Sequence[Sequence[float]], *, settings: Settings | None = None,
                     options: Options | None = None, max_gap_mm: float = 10.0) -> dict[str, Any]: ...
    def scan_regions(self, basename: str, regions_mm: Sequence[Sequence[float]], *,
                     settings: Settings | None = None, options: Options | None = None,
                     max_gap_mm: float = 10.0, progress: Callable[[str, int, int, int], bool] | None = None,
                     region_done: Callable[[int, dict[str, Any]], bool] | None = None) -> list[dict[str, Any]]: ...
    def request_cancel(self) -> None: ...
    def close(self) -> None: ...
    def __enter__(self) -> Session: ...
    def __exit__(self, exc_type: type[BaseException] | None, exc_value: BaseException | None,
                 traceback: TracebackType | None) -> None: ...
