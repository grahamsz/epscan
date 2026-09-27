//! Python bindings for the Epson API. Acquisition releases the interpreter.
use crate::{Backend, Error, Gamma, ScanOptions, ScanSettings, Session as RustSession};
use pyo3::{
    exceptions::{PyRuntimeError, PyValueError},
    prelude::*,
    types::PyDict,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    cell::RefCell,
    path::PathBuf,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pyo3::create_exception!(epscan, ScannerError, PyRuntimeError);
pyo3::create_exception!(epscan, DeviceNotFound, ScannerError);
pyo3::create_exception!(epscan, DeviceBusy, ScannerError);
pyo3::create_exception!(epscan, UnsupportedError, ScannerError);
pyo3::create_exception!(epscan, ScanCancelled, ScannerError);

impl From<Error> for PyErr {
    fn from(error: Error) -> Self {
        let message = error.to_string();
        match error {
            Error::Invalid(_) | Error::Json(_) => PyValueError::new_err(message),
            Error::NotFound(_) => DeviceNotFound::new_err(message),
            Error::Busy(_) => DeviceBusy::new_err(message),
            Error::Unsupported { .. } => UnsupportedError::new_err(message),
            Error::Cancelled => ScanCancelled::new_err(message),
            _ => ScannerError::new_err(message),
        }
    }
}
fn backend(value: &str) -> PyResult<Backend> {
    match value {
        "auto" => Ok(Backend::Auto),
        "nusb" => Ok(Backend::Nusb),
        "usbscan" => Ok(Backend::Usbscan),
        _ => Err(PyValueError::new_err(
            "backend must be auto, nusb, or usbscan",
        )),
    }
}
fn duration(value: f64, name: &str, zero_allowed: bool) -> PyResult<Duration> {
    if !value.is_finite() || value < 0.0 || (!zero_allowed && value == 0.0) {
        return Err(PyValueError::new_err(format!(
            "{name} must be finite and {}",
            if zero_allowed {
                "nonnegative"
            } else {
                "positive"
            }
        )));
    }
    let result = Duration::try_from_secs_f64(value)
        .map_err(|_| PyValueError::new_err(format!("{name} is too large")))?;
    if !zero_allowed && result.is_zero() {
        return Err(PyValueError::new_err(format!(
            "{name} is below clock precision"
        )));
    }
    Ok(result)
}
fn as_python<T: Serialize>(py: Python<'_>, value: &T) -> PyResult<Py<PyAny>> {
    let json = serde_json::to_string(value).map_err(Error::from)?;
    Ok(py.import("json")?.call_method1("loads", (json,))?.unbind())
}
fn from_dict<T: DeserializeOwned + Default>(value: Option<&Bound<'_, PyDict>>) -> PyResult<T> {
    let Some(value) = value else {
        return Ok(T::default());
    };
    let json: String = value
        .py()
        .import("json")?
        .call_method1("dumps", (value,))?
        .extract()?;
    serde_json::from_str(&json).map_err(|e| PyValueError::new_err(e.to_string()))
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Options {
    infrared: bool,
    infrared_only: bool,
    ir_depth: u8,
    ir_gamma: Gamma,
    thumbnail: bool,
    export_tiff: bool,
    banding: Option<crate::scan::banding::BandingOptions>,
    film: String,
    pass_timeout: f64,
    settle_seconds: f64,
}
impl Default for Options {
    fn default() -> Self {
        let options = ScanOptions::default();
        Self {
            infrared: options.infrared,
            infrared_only: options.infrared_only,
            ir_depth: options.ir_depth,
            ir_gamma: options.ir_gamma,
            thumbnail: options.thumbnail,
            export_tiff: options.export_tiff,
            banding: options.banding,
            film: options.film,
            pass_timeout: options.pass_timeout.as_secs_f64(),
            settle_seconds: options.settle_time.as_secs_f64(),
        }
    }
}
impl Options {
    fn into_scan_options(self) -> PyResult<ScanOptions> {
        Ok(ScanOptions {
            infrared: self.infrared,
            infrared_only: self.infrared_only,
            ir_depth: self.ir_depth,
            ir_gamma: self.ir_gamma,
            thumbnail: self.thumbnail,
            export_tiff: self.export_tiff,
            film: self.film,
            holder_selection: None,
            measure_sharpness: false,
            banding: self.banding,
            pass_timeout: duration(self.pass_timeout, "pass_timeout", false)?,
            settle_time: duration(self.settle_seconds, "settle_seconds", true)?,
        })
    }
}

/// List supported USB scanner connections without opening a scan session.
#[pyfunction(name = "list_devices", signature = (backend_name="auto"))]
fn list_devices(py: Python<'_>, backend_name: &str) -> PyResult<Py<PyAny>> {
    let backend = backend(backend_name)?;
    let devices = py.detach(|| crate::list_devices(backend))?;
    as_python(py, &devices)
}

/// Exclusive Epson connection. Use as a context manager or call close().
#[pyclass(module = "epscan")]
pub struct Session {
    inner: Mutex<RustSession>,
    cancel: AtomicBool,
}
impl Session {
    fn lock(&self) -> PyResult<MutexGuard<'_, RustSession>> {
        self.inner
            .try_lock()
            .map_err(|_| DeviceBusy::new_err("session is in use"))
    }
}
#[pymethods]
impl Session {
    #[new]
    #[pyo3(signature = (device=None, *, backend_name="auto", timeout=60.0))]
    fn new(
        py: Python<'_>,
        device: Option<String>,
        backend_name: &str,
        timeout: f64,
    ) -> PyResult<Self> {
        let backend = backend(backend_name)?;
        let timeout = duration(timeout, "timeout", false)?;
        if timeout > Duration::from_secs(214) {
            return Err(PyValueError::new_err("timeout must be at most 214 seconds"));
        }
        let session = py.detach(|| RustSession::connect(device.as_deref(), backend, timeout))?;
        Ok(Self {
            inner: Mutex::new(session),
            cancel: AtomicBool::new(false),
        })
    }
    fn capabilities(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        as_python(py, &self.lock()?.capabilities)
    }
    fn diagnostics(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let value = py.detach(|| -> PyResult<_> { Ok(self.lock()?.diagnostics()?) })?;
        as_python(py, &value)
    }
    /// Capture to numbered files. Return paths and image metadata as dictionaries.
    #[pyo3(signature = (basename, *, settings=None, options=None, progress=None))]
    fn scan(
        &self,
        py: Python<'_>,
        basename: PathBuf,
        settings: Option<&Bound<'_, PyDict>>,
        options: Option<&Bound<'_, PyDict>>,
        progress: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let settings: ScanSettings = from_dict(settings)?;
        let options = from_dict::<Options>(options)?.into_scan_options()?;
        if let Some(callback) = &progress
            && !callback.bind(py).is_callable()
        {
            return Err(PyValueError::new_err("progress must be callable"));
        }
        let result = py.detach(|| -> PyResult<_> {
            let mut session = self.lock()?;
            self.cancel.store(false, Ordering::Relaxed);
            let mut callback_error = None;
            let result = session.scan(&settings, &options, &basename, &self.cancel, &mut |p| {
                let response = Python::attach(|py| -> PyResult<bool> {
                    py.check_signals()?;
                    if let Some(callback) = &progress {
                        callback
                            .call1(py, (p.phase, p.pass, p.done, p.total))?
                            .extract::<bool>(py)
                    } else {
                        Ok(true)
                    }
                });
                match response {
                    Ok(keep_going) => keep_going,
                    Err(error) => {
                        callback_error = Some(error);
                        false
                    }
                }
            });
            if let Some(error) = callback_error {
                return Err(error);
            }
            Ok(result?)
        })?;
        as_python(py, &result)
    }
    /// Plan nearby regions as strip bounding boxes without scanner I/O.
    /// Region indices are zero-based; each batch contains its bounding-box settings.
    #[pyo3(signature = (regions_mm, *, settings=None, options=None, max_gap_mm=10.0))]
    fn plan_regions(
        &self,
        py: Python<'_>,
        regions_mm: Vec<[f64; 4]>,
        settings: Option<&Bound<'_, PyDict>>,
        options: Option<&Bound<'_, PyDict>>,
        max_gap_mm: f64,
    ) -> PyResult<Py<PyAny>> {
        let settings: ScanSettings = from_dict(settings)?;
        let options = from_dict::<Options>(options)?.into_scan_options()?;
        let plan = crate::scan::regions::plan_region_batches(
            &regions_mm,
            &settings,
            &options,
            &self.lock()?.capabilities,
            max_gap_mm,
        )?;
        as_python(py, &plan)
    }
    /// Capture nearby regions together and return extracted images in input order.
    /// region_done receives a zero-based input index and a standard result dictionary.
    #[pyo3(signature = (basename, regions_mm, *, settings=None, options=None, max_gap_mm=10.0, progress=None, region_done=None))]
    #[allow(clippy::too_many_arguments)]
    fn scan_regions(
        &self,
        py: Python<'_>,
        basename: PathBuf,
        regions_mm: Vec<[f64; 4]>,
        settings: Option<&Bound<'_, PyDict>>,
        options: Option<&Bound<'_, PyDict>>,
        max_gap_mm: f64,
        progress: Option<Py<PyAny>>,
        region_done: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let settings: ScanSettings = from_dict(settings)?;
        let options = from_dict::<Options>(options)?.into_scan_options()?;
        for (name, callback) in [("progress", &progress), ("region_done", &region_done)] {
            if let Some(callback) = callback
                && !callback.bind(py).is_callable()
            {
                return Err(PyValueError::new_err(format!("{name} must be callable")));
            }
        }
        let result = py.detach(|| -> PyResult<_> {
            let mut session = self.lock()?;
            self.cancel.store(false, Ordering::Relaxed);
            let callback_error = RefCell::new(None);
            let result = session.scan_regions(
                &regions_mm,
                &settings,
                &options,
                &basename,
                max_gap_mm,
                &self.cancel,
                &mut |p| {
                    let response = Python::attach(|py| -> PyResult<bool> {
                        py.check_signals()?;
                        if let Some(callback) = &progress {
                            callback
                                .call1(py, (p.phase, p.pass, p.done, p.total))?
                                .extract::<bool>(py)
                        } else {
                            Ok(true)
                        }
                    });
                    match response {
                        Ok(keep_going) => keep_going,
                        Err(error) => {
                            *callback_error.borrow_mut() = Some(error);
                            false
                        }
                    }
                },
                &mut |index, result| {
                    let response = Python::attach(|py| -> PyResult<bool> {
                        py.check_signals()?;
                        if let Some(callback) = &region_done {
                            callback
                                .call1(py, (index, as_python(py, result)?))?
                                .extract::<bool>(py)
                        } else {
                            Ok(true)
                        }
                    });
                    match response {
                        Ok(keep_going) => Ok(keep_going),
                        Err(error) => {
                            *callback_error.borrow_mut() = Some(error);
                            Ok(false)
                        }
                    }
                },
            );
            if let Some(error) = callback_error.into_inner() {
                return Err(error);
            }
            Ok(result?)
        })?;
        as_python(py, &result)
    }
    /// May be called by another Python thread; cancellation occurs at a transfer boundary.
    fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
    fn close(&self) -> PyResult<()> {
        self.lock()?.close();
        Ok(())
    }
    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }
    fn __exit__(
        &self,
        _ty: &Bound<'_, PyAny>,
        _value: &Bound<'_, PyAny>,
        _traceback: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        self.close()
    }
}

#[pymodule]
fn epscan(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Session>()?;
    m.add_function(wrap_pyfunction!(list_devices, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add("ScannerError", m.py().get_type::<ScannerError>())?;
    m.add("DeviceNotFound", m.py().get_type::<DeviceNotFound>())?;
    m.add("DeviceBusy", m.py().get_type::<DeviceBusy>())?;
    m.add("UnsupportedError", m.py().get_type::<UnsupportedError>())?;
    m.add("ScanCancelled", m.py().get_type::<ScanCancelled>())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::exceptions::PyZeroDivisionError;

    fn simulated(py: Python<'_>) -> (Py<Session>, ScanSettings) {
        let (scanner, settings) = crate::tests::one_rgb_session();
        (
            Py::new(
                py,
                Session {
                    inner: Mutex::new(scanner),
                    cancel: AtomicBool::new(false),
                },
            )
            .unwrap(),
            settings,
        )
    }

    #[test]
    fn dictionaries_default_and_reject_unknown_options() {
        Python::initialize();
        Python::attach(|py| {
            let settings = PyDict::new(py);
            settings.set_item("dpi", 600).unwrap();
            let parsed: ScanSettings = from_dict(Some(&settings)).unwrap();
            assert_eq!(parsed.dpi, 600);
            assert_eq!(parsed.depth, 16);
            assert_eq!(parsed.y_oversampling, 1);
            settings.set_item("y_oversampling", 3).unwrap();
            assert_eq!(
                from_dict::<ScanSettings>(Some(&settings))
                    .unwrap()
                    .y_oversampling,
                3
            );
            settings.set_item("samples", 2).unwrap();
            assert!(from_dict::<ScanSettings>(Some(&settings)).is_err());
            let options = PyDict::new(py);
            options.set_item("exposure", 2).unwrap();
            assert!(from_dict::<Options>(Some(&options)).is_err());
            assert!(duration(f64::NAN, "timeout", false).is_err());
        });
    }

    #[test]
    fn banding_dictionary_uses_native_defaults_and_rejects_unknown_fields() {
        Python::initialize();
        Python::attach(|py| {
            let options = PyDict::new(py);
            for disabled in [None, Some(py.None())] {
                if let Some(disabled) = disabled {
                    options.set_item("banding", disabled).unwrap();
                }
                assert!(
                    from_dict::<Options>(Some(&options))
                        .unwrap()
                        .into_scan_options()
                        .unwrap()
                        .banding
                        .is_none()
                );
            }
            let banding = PyDict::new(py);
            options.set_item("banding", &banding).unwrap();
            let parsed = from_dict::<Options>(Some(&options))
                .unwrap()
                .into_scan_options()
                .unwrap();
            assert_eq!(
                serde_json::to_value(parsed.banding.unwrap()).unwrap(),
                serde_json::to_value(crate::scan::banding::BandingOptions::default()).unwrap()
            );
            banding.set_item("strength", 0.4).unwrap();
            banding.set_item("save_raw", true).unwrap();
            banding
                .set_item("detection_roi", vec![0, 64, 0, 32])
                .unwrap();
            let parsed = from_dict::<Options>(Some(&options))
                .unwrap()
                .into_scan_options()
                .unwrap()
                .banding
                .unwrap();
            assert_eq!(parsed.strength, 0.4);
            assert!(parsed.save_raw);
            assert_eq!(parsed.detection_roi, Some([0, 64, 0, 32]));
            assert_eq!(parsed.max_frequencies, 3);
            banding.set_item("misspelled_option", true).unwrap();
            assert!(from_dict::<Options>(Some(&options)).is_err());
            options.set_item("banding", true).unwrap();
            assert!(from_dict::<Options>(Some(&options)).is_err());
        });
    }

    #[test]
    fn python_banding_preflights_scan_and_regions_before_files_or_scanner_io() {
        Python::initialize();
        Python::attach(|py| {
            let (scanner, original_settings) = simulated(py);
            let directory = tempfile::tempdir().unwrap();
            let settings = ScanSettings::default();
            let kwargs = PyDict::new(py);
            kwargs
                .set_item("settings", as_python(py, &settings).unwrap())
                .unwrap();
            for options in [
                serde_json::json!({"banding": {}, "export_tiff": false}),
                serde_json::json!({"banding": {}, "infrared_only": true}),
                serde_json::json!({"banding": {"strength": 1.1}}),
                serde_json::json!({"banding": {"dark_full": 0.8, "dark_off": 0.6}}),
                serde_json::json!({"banding": {"detection_roi": [0, 10000, 0, 32]}}),
                serde_json::json!({"banding": {"strength_typo": 0.4}}),
            ] {
                kwargs
                    .set_item("options", as_python(py, &options).unwrap())
                    .unwrap();
                let error = scanner
                    .bind(py)
                    .call_method("scan", (directory.path().join("invalid"),), Some(&kwargs))
                    .unwrap_err();
                assert!(error.is_instance_of::<PyValueError>(py), "{error}");
                let error = scanner
                    .bind(py)
                    .call_method(
                        "scan_regions",
                        (directory.path().join("invalid"), vec![settings.rect_mm]),
                        Some(&kwargs),
                    )
                    .unwrap_err();
                assert!(error.is_instance_of::<PyValueError>(py), "{error}");
            }
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
            assert!(scanner.borrow(py).inner.lock().unwrap().is_open());
            // The untouched simulated transport can still serve its original scan.
            scanner
                .bind(py)
                .call_method(
                    "scan",
                    (directory.path().join("valid"),),
                    Some(&region_kwargs(py, &original_settings)),
                )
                .unwrap();
        });
    }

    #[test]
    fn python_banding_preserves_three_strip_plan_for_rgb_and_gray() {
        Python::initialize();
        Python::attach(|py| {
            let (scanner, _) = simulated(py);
            let regions: Vec<_> = [2.3, 62.1, 121.5]
                .into_iter()
                .flat_map(|x| (0..6).map(move |row| [x, 16.5 + 38.0 * row as f64, 24.0, 36.0]))
                .collect();
            let kwargs = PyDict::new(py);
            kwargs
                .set_item(
                    "options",
                    as_python(py, &serde_json::json!({"banding": {}})).unwrap(),
                )
                .unwrap();
            for mode in ["rgb", "gray"] {
                kwargs
                    .set_item(
                        "settings",
                        as_python(py, &serde_json::json!({"mode": mode})).unwrap(),
                    )
                    .unwrap();
                let plan = scanner
                    .bind(py)
                    .call_method("plan_regions", (regions.clone(),), Some(&kwargs))
                    .unwrap();
                let batches = plan.get_item("batches").unwrap();
                assert_eq!(batches.len().unwrap(), 3);
                for strip in 0..3 {
                    let batch = batches.get_item(strip).unwrap();
                    assert_eq!(batch.get_item("region_indices").unwrap().len().unwrap(), 6);
                    let passes = batch.get_item("plan").unwrap().get_item("passes").unwrap();
                    assert_eq!(passes.len().unwrap(), 1);
                    assert_eq!(
                        passes
                            .get_item(0)
                            .unwrap()
                            .get_item("kind")
                            .unwrap()
                            .extract::<String>()
                            .unwrap(),
                        mode
                    );
                }
            }
        });
    }

    #[test]
    fn python_scan_exports_simulated_payload_and_returns_paths() {
        Python::initialize();
        Python::attach(|py| {
            let (scanner, settings) = simulated(py);
            let directory = tempfile::tempdir().unwrap();
            let kwargs = PyDict::new(py);
            kwargs
                .set_item("settings", as_python(py, &settings).unwrap())
                .unwrap();
            let options = PyDict::new(py);
            options.set_item("export_tiff", false).unwrap();
            kwargs.set_item("options", options).unwrap();
            kwargs
                .set_item(
                    "progress",
                    py.eval(c"lambda phase, index, done, total: True", None, None)
                        .unwrap(),
                )
                .unwrap();
            let result = scanner
                .bind(py)
                .call_method("scan", (directory.path().join("python"),), Some(&kwargs))
                .unwrap();
            let rgb = result.get_item("rgb").unwrap();
            let payload: PathBuf = rgb.get_item("payload").unwrap().extract().unwrap();
            assert_eq!(std::fs::read(payload).unwrap(), vec![0x31; 48]);
            assert!(rgb.get_item("tiff").unwrap().is_none());
            scanner.bind(py).call_method0("close").unwrap();
            scanner.bind(py).call_method0("close").unwrap();
        });
    }

    #[test]
    fn callback_cancellation_and_exceptions_close_the_session() {
        Python::initialize();
        Python::attach(|py| {
            for (code, cancelled) in [
                (c"lambda *args: False", true),
                (c"lambda *args: 1 / 0", false),
            ] {
                let (scanner, settings) = simulated(py);
                let directory = tempfile::tempdir().unwrap();
                let kwargs = PyDict::new(py);
                kwargs
                    .set_item("settings", as_python(py, &settings).unwrap())
                    .unwrap();
                kwargs
                    .set_item("progress", py.eval(code, None, None).unwrap())
                    .unwrap();
                let error = scanner
                    .bind(py)
                    .call_method("scan", (directory.path().join("cancel"),), Some(&kwargs))
                    .unwrap_err();
                if cancelled {
                    assert!(error.is_instance_of::<ScanCancelled>(py));
                } else {
                    assert!(error.is_instance_of::<PyZeroDivisionError>(py));
                }
                assert!(!scanner.borrow(py).inner.lock().unwrap().is_open());
                let manifest: serde_json::Value = serde_json::from_slice(
                    &std::fs::read(directory.path().join("cancel_1.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(manifest["complete"], false);
            }
        });
    }

    fn region_kwargs<'py>(py: Python<'py>, settings: &ScanSettings) -> Bound<'py, PyDict> {
        let kwargs = PyDict::new(py);
        kwargs
            .set_item("settings", as_python(py, settings).unwrap())
            .unwrap();
        let options = PyDict::new(py);
        options.set_item("export_tiff", false).unwrap();
        kwargs.set_item("options", options).unwrap();
        kwargs
    }

    #[test]
    fn python_region_plan_groups_eighteen_frames_without_consuming_scan_data() {
        Python::initialize();
        Python::attach(|py| {
            let (scanner, settings) = simulated(py);
            let regions: Vec<_> = [2.3, 62.1, 121.5]
                .into_iter()
                .flat_map(|x| (0..6).map(move |row| [x, 16.5 + 38.0 * row as f64, 24.0, 36.0]))
                .collect();
            let kwargs = region_kwargs(py, &settings);
            let plan = scanner
                .bind(py)
                .call_method("plan_regions", (regions,), Some(&kwargs))
                .unwrap();
            let batches = plan.get_item("batches").unwrap();
            assert_eq!(batches.len().unwrap(), 3);
            for strip in 0..3 {
                let indices: Vec<usize> = batches
                    .get_item(strip)
                    .unwrap()
                    .get_item("region_indices")
                    .unwrap()
                    .extract()
                    .unwrap();
                assert_eq!(indices, (strip * 6..strip * 6 + 6).collect::<Vec<_>>());
            }

            // The simulated transport still contains the first scan response:
            // planning must not consume a command or a byte from the scanner.
            let directory = tempfile::tempdir().unwrap();
            let result = scanner
                .bind(py)
                .call_method(
                    "scan_regions",
                    (directory.path().join("region"), vec![settings.rect_mm]),
                    Some(&kwargs),
                )
                .unwrap();
            assert_eq!(result.len().unwrap(), 1);
            let rgb = result.get_item(0).unwrap().get_item("rgb").unwrap();
            let payload: PathBuf = rgb.get_item("payload").unwrap().extract().unwrap();
            assert_eq!(std::fs::read(payload).unwrap(), vec![0x31; 48]);
            assert!(rgb.get_item("tiff").unwrap().is_none());
        });
    }

    #[test]
    fn python_region_batch_validates_every_region_and_callback_before_io() {
        Python::initialize();
        Python::attach(|py| {
            let (scanner, settings) = simulated(py);
            let directory = tempfile::tempdir().unwrap();
            let kwargs = region_kwargs(py, &settings);
            for regions in [
                vec![],
                vec![settings.rect_mm, [200.0, 0.0, 1.0, 1.0]],
                vec![settings.rect_mm, [0.0, f64::NAN, 1.0, 1.0]],
                vec![settings.rect_mm, [0.0, 0.0, -1.0, 1.0]],
            ] {
                let error = scanner
                    .bind(py)
                    .call_method(
                        "scan_regions",
                        (directory.path().join("invalid"), regions),
                        Some(&kwargs),
                    )
                    .unwrap_err();
                assert!(error.is_instance_of::<PyValueError>(py));
            }
            for callback in ["progress", "region_done"] {
                kwargs.set_item(callback, 42).unwrap();
                let error = scanner
                    .bind(py)
                    .call_method(
                        "scan_regions",
                        (directory.path().join("invalid"), vec![settings.rect_mm]),
                        Some(&kwargs),
                    )
                    .unwrap_err();
                assert!(error.is_instance_of::<PyValueError>(py));
                kwargs.del_item(callback).unwrap();
            }
            for gap in [-1.0, f64::NAN, f64::INFINITY] {
                kwargs.set_item("max_gap_mm", gap).unwrap();
                let error = scanner
                    .bind(py)
                    .call_method(
                        "scan_regions",
                        (directory.path().join("invalid"), vec![settings.rect_mm]),
                        Some(&kwargs),
                    )
                    .unwrap_err();
                assert!(error.is_instance_of::<PyValueError>(py));
            }
            kwargs.del_item("max_gap_mm").unwrap();
            for malformed in [vec![vec![1.0, 2.0, 3.0]], vec![vec![1.0; 5]]] {
                assert!(
                    scanner
                        .bind(py)
                        .call_method(
                            "scan_regions",
                            (directory.path().join("invalid"), malformed),
                            Some(&kwargs),
                        )
                        .is_err()
                );
            }
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
            assert!(scanner.borrow(py).inner.lock().unwrap().is_open());
            scanner
                .bind(py)
                .call_method("scan", (directory.path().join("valid"),), Some(&kwargs))
                .unwrap();
        });
    }

    #[test]
    fn python_region_completion_callback_uses_input_index_and_result_dictionary() {
        Python::initialize();
        Python::attach(|py| {
            let (scanner, settings) = simulated(py);
            let directory = tempfile::tempdir().unwrap();
            let kwargs = region_kwargs(py, &settings);
            let globals = PyDict::new(py);
            let seen = pyo3::types::PyList::empty(py);
            globals.set_item("seen", &seen).unwrap();
            kwargs
                .set_item(
                    "region_done",
                    py.eval(
                        c"lambda index, result: seen.append((index, result)) is None",
                        Some(&globals),
                        None,
                    )
                    .unwrap(),
                )
                .unwrap();
            let results = scanner
                .bind(py)
                .call_method(
                    "scan_regions",
                    (directory.path().join("region"), vec![settings.rect_mm]),
                    Some(&kwargs),
                )
                .unwrap();
            assert_eq!(seen.len(), 1);
            let completion = seen.get_item(0).unwrap();
            assert_eq!(
                completion.get_item(0).unwrap().extract::<usize>().unwrap(),
                0
            );
            assert!(
                completion
                    .get_item(1)
                    .unwrap()
                    .eq(results.get_item(0).unwrap())
                    .unwrap()
            );
        });
    }

    #[test]
    fn python_region_callbacks_preserve_exceptions_and_cancellation() {
        Python::initialize();
        Python::attach(|py| {
            for callback in ["progress", "region_done"] {
                for (code, cancelled) in [
                    (c"lambda *args: False", true),
                    (c"lambda *args: 1 / 0", false),
                ] {
                    let (scanner, settings) = simulated(py);
                    let directory = tempfile::tempdir().unwrap();
                    let kwargs = region_kwargs(py, &settings);
                    kwargs
                        .set_item(callback, py.eval(code, None, None).unwrap())
                        .unwrap();
                    let error = scanner
                        .bind(py)
                        .call_method(
                            "scan_regions",
                            (directory.path().join("cancel"), vec![settings.rect_mm]),
                            Some(&kwargs),
                        )
                        .unwrap_err();
                    if cancelled {
                        assert!(error.is_instance_of::<ScanCancelled>(py));
                    } else {
                        assert!(error.is_instance_of::<PyZeroDivisionError>(py));
                    }
                    assert!(!scanner.borrow(py).inner.lock().unwrap().is_open());
                }
            }
        });
    }
}
